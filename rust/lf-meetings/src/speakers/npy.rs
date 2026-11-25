//! NumPy `.npy` for one 1-D float vector, the voiceprint file format.
//!
//! The writer produces exactly `np.save(buf, float32_vector, allow_pickle=False)`.
//! The reader accepts format versions 1–3 with a 1-D little-endian `<f4` or
//! `<f8` array (all Python ever writes); anything else reads as unusable,
//! never as an error. That is narrower than `np.load` (no integer or
//! big-endian arrays) on purpose.

pub const MAX_DIM: usize = 4096;
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAGIC: &[u8] = b"\x93NUMPY";
/// `numpy.lib.format.GROWTH_AXIS_MAX_DIGITS`.
const GROWTH_AXIS_MAX_DIGITS: usize = 21;
const ARRAY_ALIGN: usize = 64;

/// `np.save` bytes of a float32 vector (format 1.0).
pub fn to_bytes(vector: &[f32]) -> Vec<u8> {
    let shape = vector.len().to_string();
    let mut header = format!("{{'descr': '<f4', 'fortran_order': False, 'shape': ({shape},), }}");
    header.push_str(&" ".repeat(GROWTH_AXIS_MAX_DIGITS.saturating_sub(shape.len())));
    // 6 magic + 2 version + 2 length bytes, then the header ending in '\n'.
    let unpadded = MAGIC.len() + 4 + header.len() + 1;
    header.push_str(&" ".repeat((ARRAY_ALIGN - unpadded % ARRAY_ALIGN) % ARRAY_ALIGN));
    header.push('\n');
    let mut out = Vec::with_capacity(MAGIC.len() + 4 + header.len() + vector.len() * 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&[1, 0]);
    out.extend_from_slice(&(header.len() as u16).to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    for value in vector {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// A finite, non-zero 1-D vector as an L2-normalised float32 copy, like
/// `collector._unit_vector` (values are cast to float32 first).
pub fn unit_vector(values: impl IntoIterator<Item = f64>) -> Option<Vec<f32>> {
    let values: Vec<f32> = values.into_iter().map(|v| v as f32).collect();
    if values.is_empty() || values.len() > MAX_DIM || !values.iter().all(|v| v.is_finite()) {
        return None;
    }
    let norm = values
        .iter()
        .map(|&v| f64::from(v) * f64::from(v))
        .sum::<f64>()
        .sqrt() as f32;
    if !norm.is_finite() || norm <= 0.0 {
        return None;
    }
    Some(values.iter().map(|v| v / norm).collect())
}

/// The unit vector stored in `data`, or `None` when it is not a usable voiceprint.
pub fn from_bytes(data: &[u8]) -> Option<Vec<f32>> {
    if data.len() > MAX_FILE_BYTES || !data.starts_with(MAGIC) || data.len() < MAGIC.len() + 2 {
        return None;
    }
    let major = data[MAGIC.len()];
    let rest = &data[MAGIC.len() + 2..];
    let (length, rest) = match major {
        1 => (
            usize::from(u16::from_le_bytes(rest.get(..2)?.try_into().ok()?)),
            &rest[2..],
        ),
        2 | 3 => (
            u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize,
            &rest[4..],
        ),
        _ => return None,
    };
    if data[MAGIC.len() + 1] != 0 {
        return None;
    }
    let header = rest.get(..length)?;
    let body = &rest[length..];
    let header = if major == 3 {
        std::str::from_utf8(header).ok()?.to_string()
    } else {
        header.iter().map(|&b| b as char).collect() // latin-1
    };
    let (descr, count) = parse_header(&header)?;
    if count == 0 || count > MAX_DIM {
        return None;
    }
    let size = if descr == 4 { 4 } else { 8 };
    let body = body.get(..count * size)?;
    let values: Vec<f64> = body
        .chunks_exact(size)
        .map(|c| match size {
            4 => f64::from(f32::from_le_bytes(c.try_into().unwrap())),
            _ => f64::from_le_bytes(c.try_into().unwrap()),
        })
        .collect();
    unit_vector(values)
}

/// `(item size, length)` from a `{'descr': ..., 'fortran_order': ..., 'shape': (N,)}`
/// header dict; `None` for anything but a 1-D `<f4`/`<f8` array.
fn parse_header(header: &str) -> Option<(usize, usize)> {
    let inner = header
        .trim_end_matches(['\n', ' ', '\0'])
        .trim()
        .strip_prefix('{')?
        .strip_suffix('}')?;
    let (mut descr, mut order, mut shape) = (None, None, None);
    let mut rest = inner.trim();
    while !rest.is_empty() {
        let (key, after) = quoted(rest)?;
        let after = after.trim_start().strip_prefix(':')?.trim_start();
        let (value, after) = if after.starts_with('(') {
            let end = after.find(')')?;
            (&after[..=end], &after[end + 1..])
        } else if after.starts_with(['\'', '"']) {
            let (text, tail) = quoted(after)?;
            let consumed = after.len() - tail.len();
            descr = descr.or((key == "descr").then(|| text.to_string()));
            (&after[..consumed], tail)
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim(), &after[end..])
        };
        match key {
            "descr" => {}
            "fortran_order" => order = Some(value),
            "shape" => shape = Some(value),
            _ => return None,
        }
        rest = after.trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    let size = match descr.as_deref()? {
        "<f4" => 4,
        "<f8" => 8,
        _ => return None,
    };
    if !matches!(order?, "False" | "True") {
        return None;
    }
    let dims = shape?.strip_prefix('(')?.strip_suffix(')')?;
    let dims: Vec<&str> = dims
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .collect();
    let [n] = dims.as_slice() else {
        return None;
    };
    Some((size, n.parse().ok()?))
}

/// A `'...'` or `"..."` literal without escapes and what follows it.
fn quoted(text: &str) -> Option<(&str, &str)> {
    let quote = text.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let end = text[1..].find(quote)? + 1;
    Some((&text[1..end], &text[end + 1..]))
}
