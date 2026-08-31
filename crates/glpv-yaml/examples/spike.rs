//! Day-1 spike: print the raw saphyr-parser event stream with spans for a file,
//! to confirm marker line/col base, tag delivery, anchors/aliases and `<<` handling.

use saphyr_parser::{Event, Parser};

fn main() {
    let path = std::env::args().nth(1).expect("usage: spike <file.yml>");
    let text = std::fs::read_to_string(&path).unwrap();
    let parser = Parser::new_from_str(&text);
    for item in parser {
        let (ev, span) = item.unwrap();
        let s = span.start;
        let e = span.end;
        match ev {
            Event::Scalar(v, style, aid, tag) => println!(
                "{}:{}..{}:{} Scalar({:?}, {:?}, anchor={}, tag={:?})",
                s.line(),
                s.col(),
                e.line(),
                e.col(),
                v,
                style,
                aid,
                tag.map(|t| format!("{}{}", t.handle, t.suffix))
            ),
            Event::SequenceStart(aid, tag) => println!(
                "{}:{}..{}:{} SeqStart(anchor={}, tag={:?})",
                s.line(),
                s.col(),
                e.line(),
                e.col(),
                aid,
                tag.map(|t| format!("{}{}", t.handle, t.suffix))
            ),
            Event::MappingStart(aid, tag) => println!(
                "{}:{}..{}:{} MapStart(anchor={}, tag={:?})",
                s.line(),
                s.col(),
                e.line(),
                e.col(),
                aid,
                tag.map(|t| format!("{}{}", t.handle, t.suffix))
            ),
            Event::Alias(aid) => println!(
                "{}:{}..{}:{} Alias({})",
                s.line(),
                s.col(),
                e.line(),
                e.col(),
                aid
            ),
            other => println!(
                "{}:{}..{}:{} {:?}",
                s.line(),
                s.col(),
                e.line(),
                e.col(),
                other
            ),
        }
    }
}
