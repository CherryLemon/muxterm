//! Byte-boundary robustness.
//!
//! A protocol parser that only works when a socket read happens to align with a
//! line boundary is not a protocol parser.  This test replays every recorded
//! capture under awkward chunkings and asserts that the event stream is identical
//! to the one produced by a single read.

use std::fs;
use std::path::{Path, PathBuf};

use tmux_protocol::parser::{Event, Parser};
use tmux_protocol::Limits;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn all_captures() -> Vec<PathBuf> {
    let dir = fixtures_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
        .collect();
    paths.sort();
    paths
}

fn parse_with_chunks(bytes: &[u8], chunk: usize) -> Vec<Event> {
    let mut parser = Parser::new(Limits::interactive());
    let mut events = Vec::new();
    if chunk == 0 {
        parser.feed(bytes, &mut events);
    } else {
        for piece in bytes.chunks(chunk) {
            parser.feed(piece, &mut events);
        }
    }
    parser.finish(&mut events);
    events
}

#[test]
fn chunking_never_changes_the_event_stream() {
    let captures = all_captures();
    if captures.is_empty() {
        eprintln!("no fixtures recorded yet; nothing to replay");
        return;
    }
    for path in &captures {
        let bytes = fs::read(path).expect("fixture is readable");
        let reference = parse_with_chunks(&bytes, 0);
        for chunk in [1usize, 2, 3, 5, 7, 13, 64, 4096] {
            let candidate = parse_with_chunks(&bytes, chunk);
            assert_eq!(
                candidate.len(),
                reference.len(),
                "{}: chunk size {chunk} produced {} events, single read produced {}",
                path.display(),
                candidate.len(),
                reference.len()
            );
            for (index, (a, b)) in candidate.iter().zip(reference.iter()).enumerate() {
                assert_eq!(
                    a,
                    b,
                    "{}: event {index} differs at chunk size {chunk}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn every_single_split_point_is_equivalent() {
    let captures = all_captures();
    if captures.is_empty() {
        return;
    }
    for path in &captures {
        let bytes = fs::read(path).expect("fixture is readable");
        // Only the small captures: this is O(n) parses per file.
        if bytes.len() > 64 * 1024 {
            continue;
        }
        let reference = parse_with_chunks(&bytes, 0);
        for split in 0..=bytes.len() {
            let mut parser = Parser::new(Limits::interactive());
            let mut events = Vec::new();
            parser.feed(&bytes[..split], &mut events);
            parser.feed(&bytes[split..], &mut events);
            parser.finish(&mut events);
            assert_eq!(
                events,
                reference,
                "{}: splitting at byte {split} changed the event stream",
                path.display()
            );
        }
    }
}

#[test]
fn a_truncated_fixture_never_panics() {
    let captures = all_captures();
    if captures.is_empty() {
        return;
    }
    for path in &captures {
        let bytes = fs::read(path).expect("fixture is readable");
        if bytes.len() > 32 * 1024 {
            continue;
        }
        for cut in 0..bytes.len() {
            let mut parser = Parser::new(Limits::strict());
            let mut events = Vec::new();
            parser.feed(&bytes[..cut], &mut events);
            // Arbitrary bytes appended after a truncation must not derail it either.
            parser.feed(b"\x1b\\%end 1\n%\n\x00\xff", &mut events);
            parser.finish(&mut events);
        }
    }
}

#[test]
fn oversized_input_is_bounded() {
    let limits = Limits {
        max_line_bytes: 128,
        ..Limits::strict()
    };
    let mut parser = Parser::new(limits);
    let mut events = Vec::new();
    // A pane that prints one enormous line must not grow our buffer without bound.
    parser.feed(&vec![b'A'; 1 << 20], &mut events);
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Desync {
                reason: tmux_protocol::parser::Desync::LineTooLong,
                ..
            }
        )),
        "an over-long line must be reported"
    );
    assert!(
        parser.buffered_bytes() <= 128,
        "buffered bytes must stay bounded, got {}",
        parser.buffered_bytes()
    );
}

#[test]
fn interleaved_output_and_responses_stay_ordered() {
    // A synthetic stream that mixes notifications with a command response; the
    // ordering guarantee is what the snapshot watermark depends on.
    let stream = b"%output %0 one\n\
                   %begin 1 1 0\n\
                   captured line\n\
                   %end 1 1 0\n\
                   %output %0 two\n\
                   %window-add @2\n\
                   %output %0 three\n";
    let mut parser = Parser::with_default_limits();
    let mut events = Vec::new();
    for chunk in stream.chunks(3) {
        parser.feed(chunk, &mut events);
    }
    let summary: Vec<String> = events
        .iter()
        .map(|event| match event {
            Event::Output { data, .. } => format!("out:{}", String::from_utf8_lossy(data)),
            Event::ResponseBegin(_) => "begin".to_string(),
            Event::ResponseLine(line) => format!("line:{}", String::from_utf8_lossy(line)),
            Event::ResponseEnd(_) => "end".to_string(),
            Event::WindowAdd { window } => format!("window:{window}"),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            "out:one",
            "begin",
            "line:captured line",
            "end",
            "out:two",
            "window:@2",
            "out:three",
        ]
    );
}

#[test]
fn invalid_utf8_is_never_lossy_converted() {
    let stream = b"%output %0 \\377\\376\\375\n";
    let mut parser = Parser::with_default_limits();
    let mut events = Vec::new();
    parser.feed(stream, &mut events);
    match &events[0] {
        Event::Output { data, .. } => assert_eq!(data, &[0xff, 0xfe, 0xfd]),
        other => panic!("unexpected {other:?}"),
    }
}
