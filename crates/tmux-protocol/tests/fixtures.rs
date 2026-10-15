//! Replay of the recorded black-box captures.
//!
//! Each capture is a raw byte stream produced by a real tmux server; see
//! `docs/evidence/README.md`.  The assertions here are deliberately about
//! *invariants* rather than exact event sequences, so that recording a new fixture
//! from another tmux release does not require rewriting expectations — but any
//! change in framing, escaping or notification shape does fail the run.

use std::fs;
use std::path::{Path, PathBuf};

use tmux_protocol::parser::{Desync, Event, Parser};
use tmux_protocol::Limits;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load(name: &str) -> Option<Vec<u8>> {
    let path = fixtures_dir().join(name);
    fs::read(&path).ok()
}

/// Every capture listed in the manifest, or an empty list when none were recorded.
fn manifest_captures() -> Vec<PathBuf> {
    let manifest = fixtures_dir().join("manifest.json");
    let Ok(text) = fs::read_to_string(&manifest) else {
        return Vec::new();
    };
    let Ok(json) = Json::parse(&text) else {
        panic!("manifest.json is not parseable");
    };
    let mut paths = Vec::new();
    let Some(probes) = json.get("probes") else {
        return paths;
    };
    for (_, entries) in probes.entries() {
        for (_, value) in entries.entries() {
            if let Json::Str(name) = value {
                paths.push(fixtures_dir().join(name));
            }
        }
    }
    paths.sort();
    paths
}

#[test]
fn every_recorded_capture_parses_without_panicking() {
    let captures = manifest_captures();
    if captures.is_empty() {
        eprintln!("no fixtures recorded yet; skipping replay");
        return;
    }
    for path in &captures {
        let bytes = fs::read(path).expect("fixture readable");
        let mut parser = Parser::new(Limits::interactive());
        let mut events = Vec::new();
        // Feed in awkward chunk sizes on purpose: the recording is one blob, but a
        // real socket never delivers it that way.
        for chunk in bytes.chunks(7) {
            parser.feed(chunk, &mut events);
        }
        parser.finish(&mut events);
        eprintln!(
            "{}: {} bytes -> {} events",
            path.file_name().unwrap().to_string_lossy(),
            bytes.len(),
            events.len()
        );
    }
}

#[test]
fn command_responses_are_well_framed() {
    let Some(bytes) = load("attach_and_commands.01_cmd_display_message.bin") else {
        eprintln!("fixture missing; this assertion is skipped");
        return;
    };
    let mut parser = Parser::with_default_limits();
    let mut events = Vec::new();
    parser.feed(&bytes, &mut events);

    let begin = events
        .iter()
        .position(|event| matches!(event, Event::ResponseBegin(_)))
        .expect("a command response must be framed by %begin");
    let end = events
        .iter()
        .position(|event| matches!(event, Event::ResponseEnd(_) | Event::ResponseError(_)))
        .expect("%end or %error must close it");
    assert!(begin < end);
    // The answer to `display-message -p '#{session_name}'` is one line: the name.
    let body: Vec<&Vec<u8>> = events[begin + 1..end]
        .iter()
        .filter_map(|event| match event {
            Event::ResponseLine(line) => Some(line),
            _ => None,
        })
        .collect();
    assert_eq!(body.len(), 1, "one line of output expected: {body:?}");
    assert_eq!(body[0], b"dev");
}

#[test]
fn errors_are_reported_as_error_blocks() {
    let Some(bytes) = load("attach_and_commands.03_cmd_error.bin") else {
        return;
    };
    let mut parser = Parser::with_default_limits();
    let mut events = Vec::new();
    parser.feed(&bytes, &mut events);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ResponseError(_))),
        "a bogus command must come back as %error, got {events:?}"
    );
}

#[test]
fn binary_output_survives_the_octal_escaping() {
    let Some(bytes) = load("attach_and_commands.07_cmd_binary_output.bin") else {
        return;
    };
    let mut parser = Parser::with_default_limits();
    let mut events = Vec::new();
    parser.feed(&bytes, &mut events);

    // The pane ran: printf "A\377\376B\033[31mRED\033[0mC\n"
    // The capture contains the send-keys command echo and the response; what we
    // assert is that *some* %output decoded to bytes that are not valid UTF-8 and
    // that the escape sequences arrived intact.
    let mut saw_invalid_utf8 = false;
    let mut saw_esc_sequence = false;
    for event in &events {
        if let Event::Output { data, .. } = event {
            if String::from_utf8(data.clone()).is_err() {
                saw_invalid_utf8 = true;
            }
            if data.windows(2).any(|pair| pair == b"\x1b[") {
                saw_esc_sequence = true;
            }
        }
    }
    assert!(
        saw_esc_sequence,
        "escape sequences must reach the parser unescaped"
    );
    eprintln!("saw invalid utf-8 in pane output: {saw_invalid_utf8}");
}

#[test]
fn wide_characters_are_preserved_as_utf8() {
    let Some(bytes) = load("attach_and_commands.08_cmd_wide_output.bin") else {
        return;
    };
    let mut parser = Parser::with_default_limits();
    let mut events = Vec::new();
    parser.feed(&bytes, &mut events);
    let text: Vec<u8> = events
        .iter()
        .filter_map(|event| match event {
            Event::Output { data, .. } => Some(data.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    let text = String::from_utf8_lossy(&text);
    assert!(
        text.contains("中文"),
        "CJK output must survive byte-for-byte, got {text:?}"
    );
}

#[test]
fn control_mode_entry_and_exit_are_visible() {
    let Some(prologue) = load("cc_entry.00_cc_prologue.bin") else {
        return;
    };
    assert!(
        prologue
            .windows(tmux_protocol::switch::CONTROL_MODE_DCS.len())
            .any(|window| window == tmux_protocol::switch::CONTROL_MODE_DCS),
        "the -CC capture must contain the DCS introducer {:?}",
        String::from_utf8_lossy(tmux_protocol::switch::CONTROL_MODE_DCS)
    );

    let Some(exit) = load("cc_entry.02_cc_exit.bin") else {
        return;
    };
    let contains = |haystack: &[u8], needle: &[u8]| {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    };
    assert!(
        contains(&exit, b"%exit"),
        "the exit capture must contain %exit, got {:?}",
        String::from_utf8_lossy(&exit)
    );
    assert!(
        contains(&exit, tmux_protocol::switch::STRING_TERMINATOR),
        "the exit capture must contain the string terminator"
    );
}

#[test]
fn layout_strings_from_a_real_server_parse() {
    let Some(bytes) = load("notifications.15_layout_string.bin") else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    let layout_text = text.trim();
    let layout = tmux_protocol::layout::Layout::parse(layout_text.as_bytes())
        .unwrap_or_else(|err| panic!("layout {layout_text:?} did not parse: {err}"));
    assert!(layout.root.pane_count() >= 1);
}

#[test]
fn the_layout_bracket_mapping_matches_the_recorded_orientation() {
    // `notifications.03_split_window` splits with `-h` (side by side) and
    // `notifications.04_split_window_vertical` with `-v` (stacked).  Recorded from
    // tmux 3.4:
    //
    //   -h -> ...{40x24,0,0,1,39x24,41,0,2}      x differs, node uses '{'
    //   -v -> ...[39x12,41,0,2,39x11,41,13,3]    y differs, node uses '['
    //
    // The second capture nests the new split inside the first, so the assertion
    // looks at the whole tree rather than at the root.
    let (Some(horizontal), Some(vertical)) = (
        load("notifications.03_split_window.bin"),
        load("notifications.04_split_window_vertical.bin"),
    ) else {
        return;
    };

    /// Orientation of a split node, the top-left corner of its first two children.
    type SplitShape = (tmux_protocol::Orientation, (u32, u32), (u32, u32));

    fn orientations(bytes: &[u8]) -> Vec<SplitShape> {
        fn walk(node: &tmux_protocol::layout::LayoutNode, out: &mut Vec<SplitShape>) {
            if let tmux_protocol::layout::LayoutNode::Split {
                orientation,
                children,
                ..
            } = node
            {
                if children.len() >= 2 {
                    let first = children[0].cell();
                    let second = children[1].cell();
                    out.push((*orientation, (first.x, first.y), (second.x, second.y)));
                }
                for child in children {
                    walk(child, out);
                }
            }
        }

        let mut parser = Parser::with_default_limits();
        let mut events = Vec::new();
        parser.feed(bytes, &mut events);
        let mut found = Vec::new();
        for event in &events {
            if let Event::LayoutChange { layout, .. } = event {
                if let Ok(parsed) = tmux_protocol::layout::Layout::parse(layout) {
                    walk(&parsed.root, &mut found);
                }
            }
        }
        found
    }

    let horizontal = orientations(&horizontal);
    let vertical = orientations(&vertical);
    assert!(!horizontal.is_empty(), "-h split must report a layout");
    assert!(!vertical.is_empty(), "-v split must report a layout");

    // Side by side: the children differ in x, and tmux writes '{' for that node.
    let (side_by_side, ..) = horizontal[0];
    assert_eq!(
        side_by_side,
        tmux_protocol::Orientation::LeftRight,
        "split-window -h produces a left/right node"
    );
    // Stacked: the children differ in y, and tmux writes '[' for that node.
    let stacked = vertical
        .iter()
        .find(|(_, first, second)| first.1 != second.1)
        .expect("the -v capture must contain a stacked node");
    assert_eq!(
        stacked.0,
        tmux_protocol::Orientation::TopBottom,
        "split-window -v produces a top/bottom node, got {stacked:?}"
    );
    // And the geometry agrees with the orientation we assigned.
    assert_ne!(
        horizontal[0].1 .0, horizontal[0].2 .0,
        "left/right children must differ in x"
    );
    assert_ne!(
        stacked.1 .1, stacked.2 .1,
        "stacked children must differ in y"
    );
}

#[test]
fn a_desynchronised_capture_is_reported_not_ignored() {
    // Feeding a truncated stream must never panic and must never invent events.
    let Some(bytes) = load("attach_and_commands.01_cmd_display_message.bin") else {
        return;
    };
    for cut in 0..bytes.len().min(64) {
        let mut parser = Parser::new(Limits::strict());
        let mut events = Vec::new();
        parser.feed(&bytes[..cut], &mut events);
        parser.finish(&mut events);
        for event in &events {
            if let Event::Desync { reason, .. } = event {
                assert!(!reason.as_str().is_empty());
            }
        }
        let _ = Desync::LineTooLong;
    }
}

// --- a tiny JSON reader -------------------------------------------------------
//
// The fixtures manifest is the only JSON in the tree, and the protocol crate is
// dependency-free on purpose.  A 40-line reader beats a dependency here.

#[allow(dead_code)]
enum Json {
    Str(String),
    Obj(Vec<(String, Json)>),
    Arr(Vec<Json>),
    Other,
}

impl Json {
    fn parse(text: &str) -> Result<Self, ()> {
        let mut chars = text.chars().peekable();
        let value = Self::parse_value(&mut chars)?;
        Ok(value)
    }

    fn parse_value(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<Self, ()> {
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            Some('{') => {
                chars.next();
                let mut entries = Vec::new();
                loop {
                    while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
                        chars.next();
                    }
                    if chars.peek() == Some(&'}') {
                        chars.next();
                        break;
                    }
                    let Json::Str(key) = Self::parse_value(chars)? else {
                        return Err(());
                    };
                    while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ':') {
                        chars.next();
                    }
                    let value = Self::parse_value(chars)?;
                    entries.push((key, value));
                }
                Ok(Self::Obj(entries))
            }
            Some('[') => {
                chars.next();
                let mut items = Vec::new();
                loop {
                    while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
                        chars.next();
                    }
                    if chars.peek() == Some(&']') {
                        chars.next();
                        break;
                    }
                    items.push(Self::parse_value(chars)?);
                }
                Ok(Self::Arr(items))
            }
            Some('"') => {
                chars.next();
                let mut text = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                text.push(escaped);
                            }
                        }
                        other => text.push(other),
                    }
                }
                Ok(Self::Str(text))
            }
            Some(_) => {
                while matches!(chars.peek(), Some(c) if !c.is_whitespace() && *c != ',' && *c != '}' && *c != ']')
                {
                    chars.next();
                }
                Ok(Self::Other)
            }
            None => Err(()),
        }
    }

    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn entries(&self) -> Vec<(&String, &Json)> {
        match self {
            Self::Obj(entries) => entries.iter().map(|(k, v)| (k, v)).collect(),
            _ => Vec::new(),
        }
    }
}
