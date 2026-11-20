//! Just enough multipart/form-data (RFC 7578) to read the send form with its file.

const MAX_PARTS: usize = 32;

pub struct Part<'a> {
    pub name: String,
    pub filename: Option<String>,
    pub data: &'a [u8],
}

impl Part<'_> {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(self.data).into_owned()
    }
}

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

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// The value of `key="..."` in a Content-Disposition header.
fn param(header: &str, key: &str) -> Option<String> {
    header.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(key)
            .then(|| v.trim().trim_matches('"').to_string())
    })
}

fn part(raw: &[u8]) -> Option<Part<'_>> {
    let end = find(raw, b"\r\n\r\n", 0)?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let disp = head.split("\r\n").find(|l| {
        l.split_once(':')
            .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("content-disposition"))
    })?;
    Some(Part {
        name: param(disp, "name")?,
        filename: param(disp, "filename"),
        data: &raw[end + 4..],
    })
}

/// The form's parts, or None if the body isn't well-formed multipart.
pub fn parse<'a>(body: &'a [u8], boundary: &str) -> Option<Vec<Part<'a>>> {
    let delim = format!("\r\n--{boundary}");
    let first = format!("--{boundary}");
    let mut pos = find(body, first.as_bytes(), 0)? + first.len();
    let mut out = vec![];
    loop {
        if body.get(pos..pos + 2)? == b"--" {
            return Some(out);
        }
        if body.get(pos..pos + 2)? != b"\r\n" {
            return None;
        }
        pos += 2;
        // the forms have a handful of fields; anything with many more isn't one of them
        if out.len() >= MAX_PARTS {
            return None;
        }
        let next = find(body, delim.as_bytes(), pos)?;
        out.push(part(&body[pos..next])?);
        pos = next + delim.len();
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

    #[test]
    fn reads_fields_and_a_file() {
        let file = b"line one\r\n--not the boundary\r\n\x00\xff";
        let b = body(file);
        let parts = parse(&b, B).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            (parts[0].name.as_str(), parts[0].text()),
            ("to", "south".into())
        );
        assert_eq!(parts[0].filename, None);
        assert_eq!(parts[1].filename.as_deref(), Some("list.txt"));
        assert_eq!(parts[1].data, file);
    }

    #[test]
    fn a_body_of_many_tiny_parts_is_refused() {
        let mut b = Vec::new();
        for _ in 0..40 {
            b.extend_from_slice(
                b"------x7

Content-Disposition: form-data; name=\"a\"



x

",
            );
        }
        b.extend_from_slice(
            b"------x7--

",
        );
        assert!(parse(&b, B).is_none());
    }

    #[test]
    fn truncated_bodies_are_refused() {
        let b = body(b"abc");
        assert!(parse(&b[..b.len() - 12], B).is_none());
        assert!(parse(b"nothing here", B).is_none());
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
