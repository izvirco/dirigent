#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Event {
    pub name: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub(super) struct Decoder {
    line: Vec<u8>,
    name: Option<String>,
    data: Option<String>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut events = vec![];
        let mut rest = bytes;
        // A line ends at `\n`, which is never part of a longer character: a line is whole
        // before it is read as text.
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&rest[..end]);
            rest = &rest[end + 1..];
            let line = std::mem::take(&mut self.line);
            events.extend(self.line(&line));
        }
        self.line.extend_from_slice(rest);
        events
    }

    pub fn finish(&mut self) -> Option<Event> {
        let line = std::mem::take(&mut self.line);
        let event = if line.is_empty() { None } else { self.line(&line) };
        event.or_else(|| self.line(b""))
    }

    fn line(&mut self, line: &[u8]) -> Option<Event> {
        let line = String::from_utf8_lossy(line);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            let (name, data) = (self.name.take(), self.data.take());
            return (name.is_some() || data.is_some()).then(|| Event { name, data: data.unwrap_or_default() });
        }
        // `field: value`; a line that starts with a colon is a comment.
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.name = Some(value.to_owned()),
            "data" => match &mut self.data {
                // An event's data may span lines.
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            },
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: Option<&str>, data: &str) -> Event {
        Event { name: name.map(str::to_owned), data: data.into() }
    }

    #[test]
    fn events_come_out_whole_however_the_bytes_are_cut() {
        let stream = "event: a\ndata: {\"x\":\"é\"}\n\n: a comment\ndata: one\r\ndata: two\r\n\r\nevent: ping\n\ndata:tight\n\n";
        let expected = [event(Some("a"), "{\"x\":\"é\"}"), event(None, "one\ntwo"), event(Some("ping"), ""), event(None, "tight")];
        for size in [stream.len(), 1, 3] {
            let mut decoder = Decoder::default();
            let events: Vec<Event> = stream.as_bytes().chunks(size).flat_map(|chunk| decoder.push(chunk)).collect();
            assert_eq!(events, expected, "in pieces of {size}");
            assert_eq!(decoder.finish(), None);
        }
    }

    #[test]
    fn a_stream_that_ends_mid_event_still_gives_it() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(b"data: first\n\ndata: la"), [event(None, "first")]);
        assert_eq!(decoder.finish(), Some(event(None, "la")));
        assert_eq!(decoder.finish(), None);
    }
}
