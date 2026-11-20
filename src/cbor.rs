//! Just enough CBOR (RFC 8949) to take the payload out of a Bundle Protocol 7 bundle
//! (RFC 9171): a bundle is an array (usually indefinite-length) of blocks, the primary
//! block first, then canonical blocks `[type, number, flags, crc type, data, crc?]`. The
//! payload block has type 1 and its data is a byte string.

use anyhow::{Result, bail, ensure};

#[derive(Debug, PartialEq)]
pub enum Item {
    Uint(u64),
    Neg(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Item>),
    Map(Vec<(Item, Item)>),
    Tag(u64, Box<Item>),
    Simple(u64),
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

const MAX_DEPTH: usize = 16;
const BREAK: u8 = 0xff;

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8> {
        let v = *self
            .b
            .get(self.at)
            .ok_or_else(|| anyhow::anyhow!("CBOR ends early"))?;
        self.at += 1;
        Ok(v)
    }

    fn take(&mut self, n: u64) -> Result<&[u8]> {
        let n = usize::try_from(n)?;
        ensure!(self.b.len() - self.at >= n, "CBOR ends early");
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }

    /// The argument of a head; None for indefinite length (additional info 31).
    fn arg(&mut self, info: u8) -> Result<Option<u64>> {
        Ok(Some(match info {
            0..=23 => info as u64,
            24 => self.byte()? as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into()?) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into()?) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into()?),
            31 => return Ok(None),
            _ => bail!("CBOR uses a reserved length encoding"),
        }))
    }

    fn at_break(&mut self) -> Result<bool> {
        if *self
            .b
            .get(self.at)
            .ok_or_else(|| anyhow::anyhow!("CBOR ends early"))?
            == BREAK
        {
            self.at += 1;
            return Ok(true);
        }
        Ok(false)
    }

    /// The pieces of an indefinite-length byte or text string.
    fn chunks(&mut self, major: u8, depth: usize) -> Result<Vec<u8>> {
        let mut out = vec![];
        while !self.at_break()? {
            match self.item(depth + 1)? {
                Item::Bytes(b) if major == 2 => out.extend(b),
                Item::Text(t) if major == 3 => out.extend(t.into_bytes()),
                _ => bail!("CBOR string chunk of the wrong type"),
            }
        }
        Ok(out)
    }

    fn item(&mut self, depth: usize) -> Result<Item> {
        ensure!(depth < MAX_DEPTH, "CBOR nested too deeply");
        let head = self.byte()?;
        let (major, info) = (head >> 5, head & 0x1f);
        let arg = self.arg(info)?;
        Ok(match (major, arg) {
            (0, Some(n)) => Item::Uint(n),
            (1, Some(n)) => Item::Neg(n),
            (2, Some(n)) => Item::Bytes(self.take(n)?.to_vec()),
            (2, None) => Item::Bytes(self.chunks(2, depth)?),
            (3, Some(n)) => Item::Text(String::from_utf8(self.take(n)?.to_vec())?),
            (3, None) => Item::Text(String::from_utf8(self.chunks(3, depth)?)?),
            (4, Some(n)) => {
                // every element takes at least a byte, so a huge count can't be real
                ensure!(
                    n as usize <= self.b.len() - self.at,
                    "CBOR array longer than its input"
                );
                let mut v = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    v.push(self.item(depth + 1)?);
                }
                Item::Array(v)
            }
            (4, None) => {
                let mut v = vec![];
                while !self.at_break()? {
                    v.push(self.item(depth + 1)?);
                }
                Item::Array(v)
            }
            (5, Some(n)) => {
                ensure!(
                    n as usize <= self.b.len() - self.at,
                    "CBOR map longer than its input"
                );
                let mut v = vec![];
                for _ in 0..n {
                    v.push((self.item(depth + 1)?, self.item(depth + 1)?));
                }
                Item::Map(v)
            }
            (5, None) => {
                let mut v = vec![];
                while !self.at_break()? {
                    v.push((self.item(depth + 1)?, self.item(depth + 1)?));
                }
                Item::Map(v)
            }
            (6, Some(t)) => Item::Tag(t, Box::new(self.item(depth + 1)?)),
            // simple values and floats; a bundle carries none we need
            (7, Some(n)) => Item::Simple(n),
            _ => bail!("CBOR break or indefinite length where none is allowed"),
        })
    }
}

pub fn decode(b: &[u8]) -> Result<Item> {
    let mut r = Reader { b, at: 0 };
    let v = r.item(0)?;
    ensure!(r.at == b.len(), "bytes left over after the CBOR item");
    Ok(v)
}

/// The payload of an RFC 9171 bundle.
pub fn bundle_payload(b: &[u8]) -> Result<Vec<u8>> {
    let Item::Array(blocks) = decode(b)? else {
        bail!("a bundle is a CBOR array of blocks");
    };
    ensure!(
        matches!(blocks.first(), Some(Item::Array(p)) if matches!(p.first(), Some(Item::Uint(7)))),
        "not a version 7 bundle"
    );
    for block in &blocks[1..] {
        if let Item::Array(f) = block
            && let (Some(Item::Uint(1)), Some(Item::Bytes(data))) = (f.first(), f.get(4))
        {
            return Ok(data.clone());
        }
    }
    bail!("the bundle has no payload block")
}

#[cfg(test)]
pub mod testing {
    /// Encode a minimal RFC 9171 bundle around `payload`, the way a BPv7 node would.
    pub fn bundle(payload: &[u8]) -> Vec<u8> {
        fn head(out: &mut Vec<u8>, major: u8, n: u64) {
            let m = major << 5;
            match n {
                0..=23 => out.push(m | n as u8),
                24..=0xff => out.extend([m | 24, n as u8]),
                0x100..=0xffff => {
                    out.push(m | 25);
                    out.extend((n as u16).to_be_bytes());
                }
                0x1_0000..=0xffff_ffff => {
                    out.push(m | 26);
                    out.extend((n as u32).to_be_bytes());
                }
                _ => {
                    out.push(m | 27);
                    out.extend(n.to_be_bytes());
                }
            }
        }
        fn eid(out: &mut Vec<u8>, ssp: &str) {
            head(out, 4, 2);
            head(out, 0, 1);
            head(out, 3, ssp.len() as u64);
            out.extend(ssp.as_bytes());
        }
        let mut b = vec![0x9f];
        // primary block: version, flags, crc type, dst, src, report-to, timestamp, lifetime
        head(&mut b, 4, 8);
        head(&mut b, 0, 7);
        head(&mut b, 0, 4);
        head(&mut b, 0, 0);
        eid(&mut b, "//tst-north-gw/ridgewarden-mule");
        eid(&mut b, "//tst-east-gw/");
        eid(&mut b, "//tst-east-gw/");
        head(&mut b, 4, 2);
        head(&mut b, 0, 690_468_652_541);
        head(&mut b, 0, 0);
        head(&mut b, 0, 259_200_000);
        // a hop-count block, then the payload block with a CRC-32
        head(&mut b, 4, 5);
        for n in [10, 2, 0, 0] {
            head(&mut b, 0, n);
        }
        b.extend([0x44, 0x82, 0x18, 0x20, 0x00]);
        head(&mut b, 4, 6);
        for n in [1, 1, 0, 2] {
            head(&mut b, 0, n);
        }
        head(&mut b, 2, payload.len() as u64);
        b.extend(payload);
        b.extend([0x44, 0xde, 0xad, 0xbe, 0xef]);
        b.push(0xff);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_payload_out_of_a_bundle() {
        let data = br#"{"kind":"mule.msg"}"#.repeat(40);
        assert_eq!(bundle_payload(&testing::bundle(&data)).unwrap(), data);
    }

    #[test]
    fn rfc_8949_examples_decode() {
        assert_eq!(decode(&[0x19, 0x03, 0xe8]).unwrap(), Item::Uint(1000));
        assert_eq!(decode(&[0x38, 0x63]).unwrap(), Item::Neg(99));
        assert_eq!(
            decode(&[0x5f, 0x42, 0x01, 0x02, 0x43, 0x03, 0x04, 0x05, 0xff]).unwrap(),
            Item::Bytes(vec![1, 2, 3, 4, 5])
        );
        assert_eq!(
            decode(&[0x83, 0x01, 0x82, 0x02, 0x03, 0x82, 0x04, 0x05]).unwrap(),
            Item::Array(vec![
                Item::Uint(1),
                Item::Array(vec![Item::Uint(2), Item::Uint(3)]),
                Item::Array(vec![Item::Uint(4), Item::Uint(5)]),
            ])
        );
    }

    #[test]
    fn broken_input_is_an_error_not_a_panic() {
        let b = testing::bundle(b"hello");
        for n in 0..b.len() {
            assert!(bundle_payload(&b[..n]).is_err());
        }
        assert!(decode(&[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(decode(&[0x81; 40]).is_err(), "too deep");
        assert!(bundle_payload(b"Nothing to receive").is_err());
    }
}
