use lf_core::symbols::pycompat::{self as py, re::*};
use regex::Regex;
use std::sync::LazyLock;

static SAFE_IMAGE: LazyLock<py::re::Regex> = LazyLock::new(|| {
    py::re::Regex::new(
        alt(vec![
            seq(vec![
                lit("![["),
                plus(seq(vec![
                    not_ahead(lit("//")),
                    pred(|c| !"[]\n:\\`<".contains(c)),
                ])),
                lit("]]"),
                not_ahead(set("([")),
            ]),
            seq(vec![
                lit("!["),
                star(pred(|c| !"[]\n\\`<".contains(c))),
                lit("]("),
                plus(seq(vec![
                    not_ahead(lit("//")),
                    pred(|c| py::is_word(c) || "-./%~".contains(c)),
                ])),
                lit(")"),
            ]),
        ]),
        0,
    )
});
static RAW_OPEN: LazyLock<py::re::Regex> = LazyLock::new(|| {
    py::re::Regex::new(
        seq(vec![
            lit("<"),
            alt(vec![lit_ci("style"), lit_ci("script")]),
            boundary(),
        ]),
        0,
    )
});
fn strip_raw_html(md: &str) -> String {
    let chars: Vec<_> = md.chars().collect();
    let mut out = String::new();
    let mut last = 0;
    for start in RAW_OPEN.find_iter(&chars) {
        if start.start < last {
            continue;
        }
        let name = seq(chars[start.start + 1..start.end]
            .iter()
            .map(|&c| {
                let lower = py::simple_lower(c);
                pred(move |c| py::simple_lower(c) == lower)
            })
            .collect());
        let close = py::re::Regex::new(seq(vec![lit("</"), name, star(space()), lit(">")]), 0);
        if let Some(end) = close.search(&chars[start.end..]) {
            out.extend(&chars[last..start.start]);
            last = start.end + end.end;
        }
    }
    out.extend(&chars[last..]);
    out
}
static AUTOLINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<((?:https?|mailto):[^\s\x1c-\x1f<>]*|[\p{L}\p{N}_.+\-]+@[\p{L}\p{N}_\-]+(?:\.[\p{L}\p{N}_\-]+)+)>").unwrap()
});
static ANGLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\]\(<([^<>\n]*)>").unwrap());
static HTML: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[A-Za-z!?/][^<>]*>").unwrap());
static INLINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\[\]\n\\`<]*)\]\([^()\n]*\)").unwrap());

pub fn strip_remote_media(markdown: &str) -> String {
    let mut md = markdown.to_string();
    loop {
        let cleaned = strip_raw_html(&md);
        let cleaned = AUTOLINK.replace_all(&cleaned, "$1");
        let cleaned = ANGLE.replace_all(&cleaned, |c: &regex::Captures<'_>| {
            format!("]({}", c[1].replace(' ', "%20"))
        });
        let cleaned = HTML.replace_all(&cleaned, "");
        let cleaned = INLINE.replace_all(&cleaned, |c: &regex::Captures<'_>| {
            if SAFE_IMAGE.fullmatch(&c[0].chars().collect::<Vec<_>>()) {
                c[0].to_string()
            } else {
                c[1].to_string()
            }
        });
        let chars: Vec<_> = cleaned.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i..].starts_with(&['!', '[']) {
                if let Some(m) = SAFE_IMAGE.match_at(&chars, i) {
                    out.extend(&chars[i..m.end]);
                    i = m.end;
                } else {
                    out.push('[');
                    i += 2;
                }
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        if out == md {
            return out.replace('<', "&lt;");
        }
        md = out;
    }
}
