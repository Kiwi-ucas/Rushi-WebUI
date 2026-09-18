//! Markdown + code highlighting, rendered as Leptos views (no raw HTML),
//! plus the small text helpers ported from the legacy JS.

use std::sync::OnceLock;

use fancy_regex::Regex;

/// Highlighted-token classes (port of highlightCode).
pub const HL_COM: &str = "hl-com";
pub const HL_STR: &str = "hl-str";
pub const HL_NUM: &str = "hl-num";
pub const HL_KW: &str = "hl-kw";

fn hl_regex() -> &'static Regex {
    static HL: OnceLock<Regex> = OnceLock::new();
    HL.get_or_init(|| {
        Regex::new(
            "(#.*$|//.*$|/\\*[\\s\\S]*?\\*/|'(?:[^'\\\\\\n]|\\\\.)*'|\"(?:[^\"\\\\\\n]|\\\\.)*\"|`(?:[^`\\\\]|\\\\.)*`|\\b(?:0x[0-9a-fA-F]+|\\d+(?:\\.\\d+)?)\\b|\\b(?:fn|let|mut|const|pub|struct|enum|impl|use|mod|trait|match|async|await|return|if|else|for|while|loop|def|class|import|from|export|new|function|var|true|false|None|Some|Ok|Err)\\b)",
        )
        .expect("HL regex")
    })
}

/// Classify a matched token into a highlight class (port of the JS if/else).
fn hl_class(tok: &str) -> &'static str {
    if tok.starts_with('#') || tok.starts_with("//") || tok.starts_with("/*") {
        HL_COM
    } else if tok.starts_with('"') || tok.starts_with('\'') || tok.starts_with('`') {
        HL_STR
    } else if tok.starts_with("0x") || tok.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        HL_NUM
    } else {
        HL_KW
    }
}

#[derive(Clone, Debug)]
pub struct HlSeg {
    pub text: String,
    pub class: Option<String>,
}

/// Highlight `src` into owned segments (plain text has class = None).
pub fn highlight(src: &str) -> Vec<HlSeg> {
    let mut segs: Vec<HlSeg> = Vec::new();
    let mut pos = 0;
    for m in hl_regex().captures_iter(src) {
        let m = match m {
            Ok(m) => m,
            Err(_) => break,
        };
        let span = match m.get(0) {
            Some(s) => (s.start(), s.end()),
            None => break,
        };
        if span.0 > pos {
            segs.push(HlSeg {
                text: src[pos..span.0].to_string(),
                class: None,
            });
        }
        let cls = hl_class(&src[span.0..span.1]);
        segs.push(HlSeg {
            text: src[span.0..span.1].to_string(),
            class: Some(cls.to_string()),
        });
        pos = span.1;
    }
    if pos < src.len() {
        segs.push(HlSeg {
            text: src[pos..].to_string(),
            class: None,
        });
    }
    segs
}

// ── markdown blocks ────────────────────────────────────────────────
#[derive(Clone, Debug)]
pub enum MdBlock {
    H1(String),
    H2(String),
    H3(String),
    Para(Vec<MdInline>),
    Hr,
    Ul(Vec<Vec<MdInline>>),
    Ol(Vec<Vec<MdInline>>),
    Code(String, String),
}

#[derive(Clone, Debug)]
pub enum MdInline {
    Text(String),
    Bold(String),
    Em(String),
    Code(String),
}

fn parse_inline(s: &str) -> Vec<MdInline> {
    let mut out: Vec<MdInline> = Vec::new();
    let rest = s;
    let mut i = 0;
    while i < rest.len() {
        let c = rest[i..].chars().next().unwrap();
        match c {
            '`' => {
                if let Some(end) = rest[i + 1..].find('`') {
                    out.push(MdInline::Code(rest[i + 1..i + 1 + end].to_string()));
                    i = i + 2 + end;
                    continue;
                }
                out.push(MdInline::Text(":".chars().next().unwrap().to_string()));
                i += 1;
            }
            '*' if rest[i + 1..].starts_with('*') => {
                // **bold**
                if let Some(end) = rest[i + 2..].find("**") {
                    out.push(MdInline::Bold(rest[i + 2..i + 2 + end].to_string()));
                    i = i + 2 + end + 2;
                    continue;
                }
                out.push(MdInline::Text("*".into()));
                i += 1;
            }
            '*' => {
                // *italic* (single star, not adjacent to another star)
                let after = &rest[i + 1..];
                let prev_char = rest[..i].chars().last();
                let next_is_star = after.starts_with('*');
                let prev_is_star = prev_char == Some('*');
                if !next_is_star && !prev_is_star {
                    if let Some(end) = after.find('*') {
                        let inner = &after[..end];
                        if !inner.starts_with('*') {
                            out.push(MdInline::Em(inner.to_string()));
                            i = i + 1 + end + 1;
                            continue;
                        }
                    }
                }
                out.push(MdInline::Text("*".into()));
                i += 1;
            }
            _ => {
                // consume run of plain chars until next special
                let mut j = i;
                while j < rest.len() {
                    let cj = rest[j..].chars().next().unwrap();
                    if cj == '`' || cj == '*' {
                        break;
                    }
                    j += cj.len_utf8();
                }
                out.push(MdInline::Text(rest[i..j].to_string()));
                i = j;
            }
        }
    }
    out
}

fn split_inline_blocks(src: &str, bullet: bool, ordered: bool) -> Vec<Vec<MdInline>> {
    src.lines()
        .map(|l| l.trim_start())
        .filter_map(|l| {
            let item = if bullet {
                let s = l.strip_prefix("- ").or_else(|| l.strip_prefix("* "));
                s.map(|s| s.to_string())
            } else if ordered {
                let s = l.split(". ").next();
                s.map(|s| s.trim_start_matches(|c: char| c.is_ascii_digit()).trim().to_string())
            } else {
                Some(l.to_string())
            };
            item.map(|t| parse_inline(&t))
        })
        .collect()
}

/// Parse markdown source into blocks (port of the legacy renderMD).
pub fn parse_md(src: &str) -> Vec<MdBlock> {
    let mut blocks: Vec<MdBlock> = Vec::new();
    // Split on fenced code blocks first.
    let re = Regex::new(r"(```[^\n]*\n[\s\S]*?```|```[^\n]*$)").unwrap();
    let mut last = 0;
    for m in re.captures_iter(src) {
        let m = match m {
            Ok(m) => m,
            Err(_) => break,
        };
        let span = match m.get(0) {
            Some(s) => (s.start(), s.end()),
            None => break,
        };
        // text before this code block
        if span.0 > last {
            blocks.extend(text_blocks(&src[last..span.0]));
        }
        let raw = &src[span.0..span.1];
        let lang = raw
            .trim_start_matches("```")
            .split('\n')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let code = raw
            .split_once('\n')
            .map(|(_, c)| c.trim_end_matches('`').trim_end_matches('\n'))
            .unwrap_or("")
            .to_string();
        blocks.push(MdBlock::Code(lang, code));
        last = span.1;
    }
    if last < src.len() {
        blocks.extend(text_blocks(&src[last..]));
    }
    blocks
}

fn text_blocks(text: &str) -> Vec<MdBlock> {
    let mut out: Vec<MdBlock> = Vec::new();
    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        // a block of consecutive list lines?
        let lines: Vec<&str> = para.lines().map(|l| l.trim()).collect();
        let all_bullet = !lines.is_empty() && lines.iter().all(|l| l.starts_with("- ") || l.starts_with("* "));
        let all_ordered = !lines.is_empty()
            && lines
                .iter()
                .all(|l| l.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false));
        if all_bullet {
            out.push(MdBlock::Ul(split_inline_blocks(para, true, false)));
        } else if all_ordered {
            out.push(MdBlock::Ol(split_inline_blocks(para, false, true)));
        } else if para.starts_with("# ") {
            out.push(MdBlock::H1(para[2..].to_string()));
        } else if para.starts_with("## ") {
            out.push(MdBlock::H2(para[3..].to_string()));
        } else if para.starts_with("### ") {
            out.push(MdBlock::H3(para[4..].to_string()));
        } else if para.starts_with("---") {
            out.push(MdBlock::Hr);
        } else {
            out.push(MdBlock::Para(parse_inline(para)));
        }
    }
    out
}

// ── small text helpers ─────────────────────────────────────────────
/// Port of briefText(): collapse whitespace, truncate to n chars.
pub fn brief_text(s: &str, n: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > n {
        let truncated: String = collapsed.chars().take(n).collect();
        format!("{truncated}…")
    } else {
        collapsed
    }
}

/// Port of summarizeArgs(): pick a representative arg string.
pub fn summarize_args(args: &serde_json::Value) -> String {
    let pref = [
        "command", "cmd", "path", "pattern", "query", "file", "url", "prompt", "description", "text",
    ];
    let obj = args.as_object();
    if let Some(o) = obj {
        for k in pref {
            if let Some(v) = o.get(k).and_then(|v| v.as_str()) {
                if !v.trim().is_empty() {
                    return clean(v, 90);
                }
            }
        }
        for v in o.values() {
            if let Some(s) = v.as_str() {
                if !s.trim().is_empty() {
                    return clean(s, 90);
                }
            }
        }
        let j = args.to_string();
        return clean(&j, 90);
    }
    clean(&args.to_string(), 90)
}

fn clean(v: &str, max: usize) -> String {
    let c: String = v.split_whitespace().collect::<Vec<_>>().join(" ");
    if c.chars().count() > max {
        let t: String = c.chars().take(max).collect();
        format!("{t}…")
    } else {
        c
    }
}

/// Port of reasoningText(): first usable text out of a reasoning item.
pub fn reasoning_text(item: &serde_json::Value) -> Option<String> {
    fn grab(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Array(items) => items
                .iter()
                .map(|p| match p {
                    serde_json::Value::String(s) => s.clone(),
                    _ => p.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(""),
            serde_json::Value::String(s) => s.clone(),
            _ => String::new(),
        }
    }
    let c = grab(item.get("content").unwrap_or(&serde_json::Value::Null));
    if !c.is_empty() {
        return Some(c);
    }
    let sm = grab(item.get("summary").unwrap_or(&serde_json::Value::Null));
    if !sm.is_empty() {
        return Some(sm);
    }
    if item.get("encrypted_content").is_some() {
        return Some("(reasoning hidden — encrypted content only)".into());
    }
    None
}

/// Unwrap nested raw wrappers (port of normalizeEvent).
pub fn normalize_event(ev: &mut serde_json::Value) -> serde_json::Value {
    let mut cur = ev.clone();
    loop {
        let is_raw = cur.get("type").and_then(|t| t.as_str()) == Some("raw");
        let val_is_obj = cur.get("value").map(|v| v.is_object()).unwrap_or(false);
        if is_raw && val_is_obj {
            match cur.get("value").cloned() {
                Some(v) => cur = v,
                None => break,
            }
        } else {
            break;
        }
    }
    *ev = cur.clone();
    cur
}
