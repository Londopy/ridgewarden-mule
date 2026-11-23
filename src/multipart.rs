//! multipart/form-data (RFC 7578), parsed as the body arrives, so a file field can be
//! written to disk a piece at a time instead of being held in memory.

/// The forms have a handful of fields; anything with many more isn't one of them.
const MAX_PARTS: usize = 32;
/// Room for a part's headers, and for whatever comes before the first boundary.
const MAX_HEAD: usize = 8 * 1024;

/// The boundary from a `multipart/form-data; boundary=...` content type.
pub fn boundary(content_type: &str) -> Option<String> {
    let (kind, params) = content_type.split_once(';')?;
    if !kind.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        let v = v.trim().trim_matches('"');
        (k.trim().eq_ignore_ascii_case("boundary") && !v.is_empty() && v.len() <= 70)
            .then(|| v.to_string())
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The value of `key="..."` in a Content-Disposition header. A quoted value may hold a ';'.
fn param(header: &str, key: &str) -> Option<String> {
    let mut parts = vec![];
    let (mut start, mut quoted) = (0, false);
    for (i, c) in header.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(&header[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&header[start..]);
    parts.into_iter().skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(key)
            .then(|| v.trim().trim_matches('"').to_string())
    })
}

#[derive(Debug, PartialEq)]
pub enum Ev {
    /// a new part; `filename` is set for a file input, empty if none was chosen
    Part {
        name: String,
        filename: Option<String>,
    },
    Data(Vec<u8>),
    /// the current part is complete
    End,
}

#[derive(Debug, PartialEq)]
enum St {
    Preamble,
    AfterDelim,
    Headers,
    Body,
    Done,
}

pub struct Parser {
    /// "\r\n--boundary"; the body is read as if it began with "\r\n", so the first
    /// boundary looks like every other one
    delim: Vec<u8>,
    buf: Vec<u8>,
    st: St,
    parts: usize,
}

impl Parser {
    pub fn new(boundary: &str) -> Parser {
        Parser {
            delim: format!("\r\n--{boundary}").into_bytes(),
            buf: b"\r\n".to_vec(),
            st: St::Preamble,
            parts: 0,
        }
    }

    /// Take the next piece of the body; returns what it completes. Data is handed out as
    /// soon as it can't be the start of a boundary, so memory stays at about one piece.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Ev>, &'static str> {
        self.buf.extend_from_slice(chunk);
        let mut out = vec![];
        loop {
            match self.st {
                St::Preamble => match find(&self.buf, &self.delim) {
                    Some(i) => {
                        self.buf.drain(..i + self.delim.len());
                        self.st = St::AfterDelim;
                    }
                    None if self.buf.len() > MAX_HEAD => return Err("no form boundary found"),
                    None => return Ok(out),
                },
                St::AfterDelim => {
                    if self.buf.len() < 2 {
                        return Ok(out);
                    }
                    match &self.buf[..2] {
                        b"--" => {
                            self.st = St::Done;
                            self.buf.clear();
                        }
                        b"\r\n" => {
                            self.buf.drain(..2);
                            self.st = St::Headers;
                        }
                        _ => return Err("broken form boundary"),
                    }
                }
                St::Headers => {
                    let Some(end) = find(&self.buf, b"\r\n\r\n") else {
                        if self.buf.len() > MAX_HEAD {
                            return Err("form part headers too long");
                        }
                        return Ok(out);
                    };
                    self.parts += 1;
                    if self.parts > MAX_PARTS {
                        return Err("too many form fields");
                    }
                    let head = std::str::from_utf8(&self.buf[..end])
                        .map_err(|_| "form part headers are not text")?;
                    let disp = head
                        .split("\r\n")
                        .find(|l| {
                            l.split_once(':').is_some_and(|(k, _)| {
                                k.trim().eq_ignore_ascii_case("content-disposition")
                            })
                        })
                        .ok_or("form part without a name")?;
                    out.push(Ev::Part {
                        name: param(disp, "name").ok_or("form part without a name")?,
                        filename: param(disp, "filename"),
                    });
                    self.buf.drain(..end + 4);
                    self.st = St::Body;
                }
                St::Body => match find(&self.buf, &self.delim) {
                    Some(i) => {
                        if i > 0 {
                            out.push(Ev::Data(self.buf[..i].to_vec()));
                        }
                        out.push(Ev::End);
                        self.buf.drain(..i + self.delim.len());
                        self.st = St::AfterDelim;
                    }
                    None => {
                        // keep what could be the start of a boundary split across pieces
                        let keep = self.delim.len() - 1;
                        if self.buf.len() > keep {
                            let n = self.buf.len() - keep;
                            out.push(Ev::Data(self.buf.drain(..n).collect()));
                        }
                        return Ok(out);
                    }
                },
                St::Done => {
                    self.buf.clear();
                    return Ok(out);
                }
            }
        }
    }

    /// The closing boundary has been read; whatever follows it is ignored.
    pub fn done(&self) -> bool {
        self.st == St::Done
    }

    /// The body ended; it must have ended with the closing boundary.
    pub fn finish(&self) -> Result<(), &'static str> {
        if self.st == St::Done {
            Ok(())
        } else {
            Err("the form ended early")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: &str = "----x7";

    fn body(file: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(
            b"------x7\r\nContent-Disposition: form-data; name=\"to\"\r\n\r\nsouth\r\n\
              ------x7\r\nContent-Disposition: form-data; name=\"file\"; filename=\"list.txt\"\r\n\
              Content-Type: text/plain\r\n\r\n",
        );
        b.extend_from_slice(file);
        b.extend_from_slice(b"\r\n------x7--\r\n");
        b
    }

    type Parts = Vec<(String, Option<String>, Vec<u8>)>;

    /// Feed `b` in pieces of `n` bytes and put the parts back together.
    fn parts(b: &[u8], n: usize) -> Result<Parts, &'static str> {
        let mut p = Parser::new(B);
        let mut out: Parts = vec![];
        for c in b.chunks(n) {
            for ev in p.feed(c)? {
                match ev {
                    Ev::Part { name, filename } => out.push((name, filename, vec![])),
                    Ev::Data(d) => out.last_mut().unwrap().2.extend(d),
                    Ev::End => {}
                }
            }
        }
        p.finish()?;
        Ok(out)
    }

    #[test]
    fn reads_fields_and_a_file_however_the_body_is_split() {
        // pieces of the boundary, but never the whole "\r\n------x7", which a body can't hold
        let file = b"line one\r\n--not the boundary\r\n------x\x00\xff\r\n------x almost\r\n-";
        let b = body(file);
        for n in [1, 2, 3, 7, 13, 64, b.len()] {
            let p = parts(&b, n).unwrap();
            assert_eq!(p.len(), 2, "pieces of {n}");
            assert_eq!(p[0], ("to".into(), None, b"south".to_vec()));
            assert_eq!(
                p[1],
                ("file".into(), Some("list.txt".into()), file.to_vec()),
                "pieces of {n}"
            );
        }
    }

    #[test]
    fn data_is_handed_out_as_it_arrives() {
        let mut p = Parser::new(B);
        let head =
            b"------x7\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\n";
        p.feed(head).unwrap();
        let evs = p.feed(&[b'z'; 100_000]).unwrap();
        let got: usize = evs
            .iter()
            .map(|e| match e {
                Ev::Data(d) => d.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(
            got,
            100_000 - (B.len() + 3),
            "only a boundary's length is held back"
        );
    }

    #[test]
    fn truncated_or_broken_bodies_are_refused() {
        let b = body(b"abc");
        assert!(parts(&b[..b.len() - 12], 5).is_err());
        assert!(parts(b"nothing here", 4).is_err());
        assert!(parts(&vec![b'x'; MAX_HEAD + 10], 100).is_err());
    }

    #[test]
    fn a_body_of_many_tiny_parts_is_refused() {
        let mut b = Vec::new();
        for _ in 0..40 {
            b.extend_from_slice(
                b"------x7\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nx\r\n",
            );
        }
        b.extend_from_slice(b"------x7--\r\n");
        assert_eq!(parts(&b, 50).unwrap_err(), "too many form fields");
    }

    #[test]
    fn a_quoted_file_name_may_hold_a_semicolon() {
        let h = "form-data; name=\"file\"; filename=\"a;b.pdf\"";
        assert_eq!(param(h, "filename").as_deref(), Some("a;b.pdf"));
        assert_eq!(param(h, "name").as_deref(), Some("file"));
    }

    #[test]
    fn boundary_from_content_type() {
        assert_eq!(
            boundary("multipart/form-data; boundary=----WebKitFormBoundaryAbc").as_deref(),
            Some("----WebKitFormBoundaryAbc")
        );
        assert_eq!(
            boundary("multipart/form-data; boundary=\"q r\"").as_deref(),
            Some("q r")
        );
        assert_eq!(boundary("application/x-www-form-urlencoded"), None);
    }
}
