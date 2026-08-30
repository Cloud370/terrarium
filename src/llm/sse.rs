//! Incremental server-sent-events decoder.
//!
//! The decoder is a pure byte accumulator: the transport feeds it response
//! chunks and collects complete events. It follows the SSE field rules that
//! matter for the three supported protocols: `\r\n`, `\n`, and `\r` line
//! breaks, `event:`/`data:` fields with one optional leading space, multiple
//! `data:` lines joined by newlines, and `:`-prefixed comment lines.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event_name: Option<String>,
    data_lines: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one response chunk and append any completed events to `out`.
    /// Consumed bytes are dropped in one move per feed, so a chunk holding
    /// many lines costs one compact rather than one per line.
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) {
        self.buffer.extend_from_slice(chunk);
        let mut consumed = 0usize;
        while let Some((line_len, break_len)) = self.next_line_len(consumed) {
            // Only a blank line dispatches the accumulated event; anything
            // else keeps accumulating fields.
            let blank = line_len == 0;
            if !blank {
                match Self::parse_line(&self.buffer[consumed..consumed + line_len]) {
                    Line::Event(name) => self.event_name = Some(name),
                    Line::Data(data) => self.data_lines.push(data),
                    Line::Ignore => {}
                }
            }
            consumed += line_len + break_len;
            if blank {
                if let Some(event) = self.flush_pending() {
                    out.push(event);
                }
            }
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
    }

    /// Flush a trailing event that was not terminated by a blank line.
    /// `feed` already consumed every complete line, so at most a partial line
    /// remains — plus a trailing `\r` that may have been half of a `\r\n`.
    pub fn finish(&mut self, out: &mut Vec<SseEvent>) {
        if self.buffer.last() == Some(&b'\r') {
            self.buffer.pop();
        }
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            match Self::parse_line(&line) {
                Line::Event(name) => self.event_name = Some(name),
                Line::Data(data) => self.data_lines.push(data),
                Line::Ignore => {}
            }
        }
        if let Some(event) = self.flush_pending() {
            out.push(event);
        }
    }

    /// Find the next complete line at or after `start`, returning its length
    /// and break length. A trailing `\r` at the buffer edge is left pending
    /// until more bytes arrive (or `finish`) because it may be half of a
    /// `\r\n` pair.
    fn next_line_len(&self, start: usize) -> Option<(usize, usize)> {
        let buffer = &self.buffer[start..];
        for (offset, byte) in buffer.iter().enumerate() {
            match byte {
                b'\n' => return Some((offset, 1)),
                b'\r' => {
                    if offset + 1 == buffer.len() {
                        return None;
                    }
                    let break_len = if buffer[offset + 1] == b'\n' { 2 } else { 1 };
                    return Some((offset, break_len));
                }
                _ => {}
            }
        }
        None
    }

    fn parse_line(line: &[u8]) -> Line {
        if line.is_empty() || line[0] == b':' {
            return Line::Ignore;
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return Line::Ignore;
        };
        let field = &line[..colon];
        let mut value = &line[colon + 1..];
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        let value = String::from_utf8_lossy(value).into_owned();
        match field {
            b"event" => Line::Event(value),
            b"data" => Line::Data(value),
            _ => Line::Ignore,
        }
    }

    fn flush_pending(&mut self) -> Option<SseEvent> {
        if self.data_lines.is_empty() {
            // An event field without data carries no payload; drop it.
            self.event_name = None;
            return None;
        }
        Some(SseEvent {
            event: self.event_name.take(),
            data: std::mem::take(&mut self.data_lines).join("\n"),
        })
    }
}

enum Line {
    Event(String),
    Data(String),
    Ignore,
}

#[cfg(test)]
mod tests {
    use super::{SseDecoder, SseEvent};

    fn collect(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in chunks {
            decoder.feed(chunk, &mut events);
        }
        decoder.finish(&mut events);
        events
    }

    #[test]
    fn parses_unnamed_data_events() {
        let events = collect(&[b"data: {\"a\":1}\n\ndata: [DONE]\n\n"]);
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "{\"a\":1}".into()
                },
                SseEvent {
                    event: None,
                    data: "[DONE]".into()
                }
            ]
        );
    }

    #[test]
    fn parses_named_events_with_multi_data_lines() {
        let events = collect(&[b"event: message_start\ndata: {\"b\":2}\ndata: more\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("message_start".into()),
                data: "{\"b\":2}\nmore".into()
            }]
        );
    }

    #[test]
    fn handles_chunk_splits_across_line_and_crlf_boundaries() {
        let events = collect(&[
            b"event: content_block_de",
            b"lta\ndata: {\"c\"",
            b":3}\r\n\r",
            b"\ndata: tail\n\n",
        ]);
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("content_block_delta".into()),
                    data: "{\"c\":3}".into()
                },
                SseEvent {
                    event: None,
                    data: "tail".into()
                }
            ]
        );
    }

    #[test]
    fn skips_comments_and_strips_exactly_one_leading_space() {
        let events = collect(&[b": keep-alive\ndata:   spaced\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "  spaced".into()
            }]
        );
    }

    #[test]
    fn bare_cr_line_breaks_dispatch() {
        let events = collect(&[b"event: e\rdata: x\r".as_ref(), b"\rdata: y\r\r"]);
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("e".into()),
                    data: "x".into()
                },
                SseEvent {
                    event: None,
                    data: "y".into()
                }
            ]
        );
    }

    #[test]
    fn trailing_partial_line_flushes_on_finish() {
        let events = collect(&[b"data: partial"]);
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "partial".into()
            }]
        );
    }
}
