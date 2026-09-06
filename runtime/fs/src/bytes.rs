//! `runtime.bytes` — immutable byte buffers.
//!
//! A `Bytes` value is a `Foreign::Handle` wrapped in an object (the
//! channel/net handle pattern), so it is opaque to TS and travels between
//! realms by shared core. It is not an array and cannot be indexed; `at`
//! reads a single byte.
//!
//! The codecs are plain functions over `&[u8]`/`&str` with the natives as
//! thin wrappers, so the padding and validation edge cases are unit-tested
//! without a Realm.

use std::sync::Arc;
use tsr_memory::Value;
use tsr_realm::{NativeArgs, Realm, RtError};

use crate::str_arg;

pub(crate) const BYTES_KIND: &str = "bytes";

pub(crate) fn make_bytes(realm: &mut Realm, data: Arc<Vec<u8>>) -> Value {
    let f = realm
        .heap
        .alloc_foreign(tsr_memory::Foreign::Handle(BYTES_KIND, data));
    let obj = realm.heap.alloc_obj_host();
    realm.heap.obj_set(obj, Arc::from("__bytes"), Value::foreign(f));
    Value::object(obj)
}

pub(crate) fn bytes_of(realm: &Realm, v: Value, who: &str) -> Result<Arc<Vec<u8>>, RtError> {
    let err = || RtError::new(format!("{who}: expected a Bytes handle"));
    let obj = v.as_object().ok_or_else(err)?;
    let f = realm
        .heap
        .obj(obj)
        .get("__bytes")
        .and_then(|v| v.as_foreign())
        .ok_or_else(err)?;
    match realm.heap.foreign(f) {
        tsr_memory::Foreign::Handle(BYTES_KIND, any) => {
            any.clone().downcast::<Vec<u8>>().map_err(|_| err())
        }
        _ => Err(err()),
    }
}

/// `bytes_of` for argument `i`.
fn arg_bytes(
    realm: &Realm,
    args: NativeArgs,
    i: usize,
    who: &str,
) -> Result<Arc<Vec<u8>>, RtError> {
    bytes_of(realm, args.get(realm, i), who)
}

// ---------------- codecs (pure) ----------------

pub fn to_hex(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(b.len() * 2);
    for &byte in b {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

pub fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.as_bytes();
    if s.len() % 2 != 0 {
        return Err(format!("odd-length hex string ({} chars)", s.len()));
    }
    let digit = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("invalid hex character {:?}", c as char)),
        }
    };
    s.chunks(2)
        .map(|p| Ok((digit(p[0])? << 4) | digit(p[1])?))
        .collect()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn to_base64(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((*c.get(1).unwrap_or(&0) as u32) << 8)
            | (*c.get(2).unwrap_or(&0) as u32);
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 {
            B64[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn b64_value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// RFC 4648 standard alphabet, padding required.
///
/// Rejects a bad length, an unknown character, and misplaced padding.
/// ponytail: non-canonical trailing bits (`"Zg=="` vs `"Zh=="`) are
/// accepted, as in every mainstream decoder; tighten only if a caller
/// needs canonical-form validation.
pub fn from_base64(s: &str) -> Result<Vec<u8>, String> {
    let s = s.as_bytes();
    if s.len() % 4 != 0 {
        return Err(format!("base64 length {} is not a multiple of 4", s.len()));
    }
    let chunks = s.len() / 4;
    let mut out = Vec::with_capacity(chunks * 3);
    for (ci, chunk) in s.chunks(4).enumerate() {
        let last = ci + 1 == chunks;
        let mut pad = 0usize;
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                // padding is only ever the last one or two slots of the
                // final chunk
                if !last || i < 2 {
                    return Err("misplaced '=' padding".into());
                }
                pad += 1;
                n <<= 6;
            } else {
                if pad > 0 {
                    return Err("data after '=' padding".into());
                }
                match b64_value(c) {
                    Some(v) => n = (n << 6) | v,
                    None => return Err(format!("invalid base64 character {:?}", c as char)),
                }
            }
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// First offset of `needle` in `hay`, or `None`.
///
/// ponytail: naive O(n*m) scan. Swap in two-way or `memchr` if a binary
/// parser ever leans on this in a hot loop.
pub fn index_of(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---------------- natives ----------------

pub fn members(realm: &mut Realm) -> Vec<(&'static str, Value)> {
    let size = realm.add_native(|realm, args| {
        let b = arg_bytes(realm, args, 0, "bytes.size")?;
        Ok(Value::number(b.len() as f64))
    });
    let slice = realm.add_native(|realm, args| {
        let b = arg_bytes(realm, args, 0, "bytes.slice")?;
        let from = args.get(realm, 1);
        let to = args.get(realm, 2);
        let from = if from.is_number() {
            from.as_number().max(0.0) as usize
        } else {
            0
        };
        let to = if to.is_number() {
            (to.as_number().max(0.0) as usize).min(b.len())
        } else {
            b.len()
        };
        let out: Vec<u8> = b.get(from..to.max(from)).unwrap_or(&[]).to_vec();
        Ok(make_bytes(realm, Arc::new(out)))
    });
    let to_string = realm.add_native(|realm, args| {
        let b = arg_bytes(realm, args, 0, "bytes.toString")?;
        let s = String::from_utf8_lossy(&b).into_owned();
        Ok(realm.alloc_string(&s))
    });
    let from_string = realm.add_native(|realm, args| {
        let s = str_arg(realm, args, 0, "bytes.fromString")?;
        Ok(make_bytes(realm, Arc::new(s.into_bytes())))
    });

    // strict counterpart to the lossy `toString`, matching fs.readFile
    let decode = realm.add_native(|realm, args| {
        let b = arg_bytes(realm, args, 0, "bytes.decode")?;
        let s = std::str::from_utf8(&b)
            .map_err(|e| RtError::new(format!("bytes.decode: invalid UTF-8: {e}")))?
            .to_string();
        Ok(realm.alloc_string(&s))
    });
    let at = realm.add_native(|realm, args| {
        let b = arg_bytes(realm, args, 0, "bytes.at")?;
        let i = args.get(realm, 1);
        if !i.is_number() {
            return Err(RtError::new("bytes.at: expected an index"));
        }
        let i = i.as_number();
        if i < 0.0 || i.fract() != 0.0 {
            return Ok(Value::UNDEFINED);
        }
        Ok(match b.get(i as usize) {
            Some(&byte) => Value::number(byte as f64),
            None => Value::UNDEFINED,
        })
    });
    let concat = realm.add_native(|realm, args| {
        let a = arg_bytes(realm, args, 0, "bytes.concat")?;
        let b = arg_bytes(realm, args, 1, "bytes.concat")?;
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(&a);
        out.extend_from_slice(&b);
        Ok(make_bytes(realm, Arc::new(out)))
    });
    let equals = realm.add_native(|realm, args| {
        let a = arg_bytes(realm, args, 0, "bytes.equals")?;
        let b = arg_bytes(realm, args, 1, "bytes.equals")?;
        Ok(Value::bool(a == b))
    });
    let index_of_fn = realm.add_native(|realm, args| {
        let hay = arg_bytes(realm, args, 0, "bytes.indexOf")?;
        let needle = arg_bytes(realm, args, 1, "bytes.indexOf")?;
        Ok(Value::number(match index_of(&hay, &needle) {
            Some(i) => i as f64,
            None => -1.0,
        }))
    });
    let to_hex_fn = realm.add_native(|realm, args| {
        let b = arg_bytes(realm, args, 0, "bytes.toHex")?;
        let s = to_hex(&b);
        Ok(realm.alloc_string(&s))
    });
    let from_hex_fn = realm.add_native(|realm, args| {
        let s = str_arg(realm, args, 0, "bytes.fromHex")?;
        let b = from_hex(&s).map_err(|e| RtError::new(format!("bytes.fromHex: {e}")))?;
        Ok(make_bytes(realm, Arc::new(b)))
    });
    let to_base64_fn = realm.add_native(|realm, args| {
        let b = arg_bytes(realm, args, 0, "bytes.toBase64")?;
        let s = to_base64(&b);
        Ok(realm.alloc_string(&s))
    });
    let from_base64_fn = realm.add_native(|realm, args| {
        let s = str_arg(realm, args, 0, "bytes.fromBase64")?;
        let b = from_base64(&s).map_err(|e| RtError::new(format!("bytes.fromBase64: {e}")))?;
        Ok(make_bytes(realm, Arc::new(b)))
    });
    let from_array = realm.add_native(|realm, args| {
        let arr = args
            .get(realm, 0)
            .as_array()
            .ok_or_else(|| RtError::new("bytes.fromArray: expected an array"))?;
        // copy out of the heap before allocating anything
        let vals = realm.heap.arr(arr).to_vec();
        let mut out = Vec::with_capacity(vals.len());
        for (i, v) in vals.iter().enumerate() {
            if !v.is_number() {
                return Err(RtError::new(format!(
                    "bytes.fromArray: element {i} is {}, expected a number",
                    v.type_of()
                )));
            }
            let n = v.as_number();
            if n.fract() != 0.0 || !(0.0..=255.0).contains(&n) {
                return Err(RtError::new(format!(
                    "bytes.fromArray: element {i} is {n}, expected an integer in 0..=255"
                )));
            }
            out.push(n as u8);
        }
        Ok(make_bytes(realm, Arc::new(out)))
    });
    let to_array = realm.add_native(|realm, args| {
        let b = arg_bytes(realm, args, 0, "bytes.toArray")?;
        let vals: Vec<Value> = b.iter().map(|&x| Value::number(x as f64)).collect();
        Ok(Value::array(realm.heap.alloc_arr_host(&vals)))
    });

    vec![
        ("size", size),
        ("slice", slice),
        ("toString", to_string),
        ("fromString", from_string),
        ("decode", decode),
        ("at", at),
        ("concat", concat),
        ("equals", equals),
        ("indexOf", index_of_fn),
        ("toHex", to_hex_fn),
        ("fromHex", from_hex_fn),
        ("toBase64", to_base64_fn),
        ("fromBase64", from_base64_fn),
        ("fromArray", from_array),
        ("toArray", to_array),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 section 10 test vectors — the padding boundaries are the
    /// whole difficulty of base64.
    const VECTORS: [(&str, &str); 7] = [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ];

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (plain, encoded) in VECTORS {
            assert_eq!(to_base64(plain.as_bytes()), encoded, "encoding {plain:?}");
            assert_eq!(
                from_base64(encoded).unwrap(),
                plain.as_bytes(),
                "decoding {encoded:?}"
            );
        }
    }

    #[test]
    fn base64_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(from_base64(&to_base64(&all)).unwrap(), all);
    }

    #[test]
    fn base64_rejects_bad_input() {
        // a bad length is not silently truncated
        assert!(from_base64("Zg=").is_err());
        assert!(from_base64("Zm9vY").is_err());
        // an unknown character is not silently skipped
        assert!(from_base64("Zg!=").is_err());
        assert!(from_base64("Zm 9v").is_err());
        // padding in the wrong place, or data after it
        assert!(from_base64("=Zm9").is_err());
        assert!(from_base64("Z=g=").is_err());
        assert!(from_base64("Zg==Zg==").is_err(), "padding mid-string");
    }

    #[test]
    fn hex_round_trips_and_rejects_bad_input() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(to_hex(&[0, 15, 16, 255]), "000f10ff");
        assert_eq!(from_hex(&to_hex(&all)).unwrap(), all);
        assert_eq!(from_hex("DEADBEEF").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(from_hex("").unwrap(), Vec::<u8>::new());
        assert!(from_hex("abc").is_err(), "odd length");
        assert!(from_hex("zz").is_err(), "not a hex digit");
        assert!(from_hex("0x1f").is_err());
    }

    #[test]
    fn index_of_finds_and_misses() {
        let hay = b"hello world";
        assert_eq!(index_of(hay, b"hello"), Some(0));
        assert_eq!(index_of(hay, b"world"), Some(6));
        assert_eq!(index_of(hay, b"o w"), Some(4));
        assert_eq!(index_of(hay, b"nope"), None);
        // a needle longer than the haystack must not panic
        assert_eq!(index_of(b"hi", b"hello"), None);
        // windows(0) panics, so the empty needle is special-cased
        assert_eq!(index_of(hay, b""), Some(0));
        assert_eq!(index_of(b"", b""), Some(0));
    }
}
