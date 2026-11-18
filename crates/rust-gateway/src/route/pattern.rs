//! A port of `path-to-regexp` 8.4.2 `match()`, as the Reference Gateway calls it:
//! `match(path, { decode: decodeURIComponent })` with every other option at its default
//! (`delimiter: "/"`, `sensitive: false`, `end: true`, `trailing: true`).
//!
//! The library compiles a pattern into a JavaScript regular expression. This port follows
//! its tokenizer (`parse`), its expansion of optional groups (`flatten`) and its regular
//! expression builder (`toRegExpSource`, `negate`) step by step, and produces the same
//! regular expression source text. Instead of handing that text to a regex engine (the
//! generated expressions use lookahead, which the `regex` crate does not support), it keeps
//! the same structure as data and runs it with a small backtracking matcher that has the
//! semantics of a JavaScript `RegExp` with the `i` flag and without the `u` flag: input is
//! read as UTF-16 code units, and letters are compared with the ECMAScript `Canonicalize`
//! operation for non-Unicode case-insensitive matching.
//!
//! Golden vectors produced by the installed library live in `testdata/path-to-regexp.json`
//! (generator: `testdata/generate-path-to-regexp.mjs`).

use std::fmt;

use super::uri::{UriError, decode_uri_component};

mod id_tables;

/// The delimiter `path-to-regexp` uses by default.
const DELIMITER: &str = "/";

/// The most alternatives a pattern may expand to through optional groups.
const MAX_COMBINATIONS: usize = 256;

/// A pattern `path-to-regexp` rejects.
///
/// The message is the library's own, including the original path and the trailing
/// `visit https://git.new/pathToRegexpError for info`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PathError {
    message: String,
}

impl PathError {
    fn new(message: impl Into<String>, original_path: &str) -> Self {
        let mut message = message.into();
        if !original_path.is_empty() {
            message.push_str(": ");
            message.push_str(original_path);
        }
        message.push_str("; visit https://git.new/pathToRegexpError for info");
        Self { message }
    }

    /// The library's error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// A matched parameter that could not be percent-decoded.
///
/// `decodeURIComponent` throws `URIError: URI malformed` in the reference, so the lookup
/// that triggered it fails instead of matching or not matching.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("parameter `{name}` cannot be decoded: {source}")]
pub struct ParamDecodeError {
    /// The parameter's name.
    pub name: String,
    /// Why decoding failed.
    #[source]
    pub source: UriError,
}

/// The kind of a captured parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// `:name`, one segment.
    Param,
    /// `*name`, one or more segments.
    Wildcard,
}

/// A parameter of a pattern, in the order the library numbers its capture groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternKey {
    /// The parameter's name.
    pub name: String,
    /// Whether it is a `:param` or a `*wildcard`.
    pub kind: ParamKind,
}

/// The decoded value of a matched parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamValue {
    /// A `:param`, decoded with `decodeURIComponent`.
    Single(String),
    /// A `*wildcard`, split on `/` and each part decoded.
    Segments(Vec<String>),
}

/// The result of a successful match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMatch {
    /// The matched path (always the whole input: the pattern is anchored at both ends).
    pub path: String,
    /// Parameters in capture-group order. A later parameter with the same name replaces an
    /// earlier one in place.
    pub params: Vec<(String, ParamValue)>,
}

/// A compiled `path-to-regexp` pattern.
#[derive(Debug, Clone)]
pub struct PathPattern {
    source: String,
    keys: Vec<PatternKey>,
    alternatives: Vec<Vec<Elem>>,
}

impl PathPattern {
    /// Compiles one pattern, as `match(path)` does.
    pub fn compile(path: &str) -> Result<Self, PathError> {
        Self::compile_all(&[path])
    }

    /// Compiles several patterns into one, as `match([a, b, ...])` does: the result
    /// matches what any of them matches, trying them in order.
    pub fn compile_all<S: AsRef<str>>(paths: &[S]) -> Result<Self, PathError> {
        let mut builder = Builder::default();
        for path in paths {
            builder.process(path.as_ref())?;
        }
        // `(?:)` with no alternatives still matches the empty string.
        if builder.alternatives.is_empty() {
            builder.alternatives.push(Vec::new());
        }
        let source = format!("^(?:{})(?:{}$)?$", builder.source, escape(DELIMITER));
        Ok(Self {
            source,
            keys: builder.keys,
            alternatives: builder.alternatives,
        })
    }

    /// The source text of the regular expression the library would build.
    pub fn regexp_source(&self) -> &str {
        &self.source
    }

    /// The pattern's parameters, one per capture group.
    pub fn keys(&self) -> &[PatternKey] {
        &self.keys
    }

    /// Whether `input` matches, ignoring parameter decoding.
    pub fn is_match(&self, input: &str) -> bool {
        let units = Units::new(input);
        self.alternatives.iter().any(|alternative| {
            let mut caps = vec![None; self.keys.len()];
            match_elems(
                alternative,
                0,
                0,
                &units,
                &mut caps,
                &mut Failures::default(),
            )
        })
    }

    /// Runs the matcher as the library's `match` function does: `Ok(None)` when the input
    /// does not match, an error when a matched parameter cannot be decoded.
    pub fn matches(&self, input: &str) -> Result<Option<PathMatch>, ParamDecodeError> {
        let units = Units::new(input);
        for alternative in &self.alternatives {
            let mut caps = vec![None; self.keys.len()];
            if !match_elems(
                alternative,
                0,
                0,
                &units,
                &mut caps,
                &mut Failures::default(),
            ) {
                continue;
            }
            let mut params: Vec<(String, ParamValue)> = Vec::new();
            for (key, cap) in self.keys.iter().zip(&caps) {
                let Some((start, end)) = *cap else { continue };
                let raw = String::from_utf16_lossy(&units.original[start..end]);
                let decode = |value: &str| {
                    decode_uri_component(value).map_err(|source| ParamDecodeError {
                        name: key.name.clone(),
                        source,
                    })
                };
                let value = match key.kind {
                    ParamKind::Param => ParamValue::Single(decode(&raw)?),
                    ParamKind::Wildcard => ParamValue::Segments(
                        raw.split(DELIMITER)
                            .map(decode)
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                };
                match params.iter_mut().find(|(name, _)| *name == key.name) {
                    Some(slot) => slot.1 = value,
                    None => params.push((key.name.clone(), value)),
                }
            }
            return Ok(Some(PathMatch {
                path: input.to_string(),
                params,
            }));
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------------------
// Tokenizer (`parse`).

#[derive(Debug, Clone)]
enum Token {
    Text(String),
    Param(String),
    Wildcard(String),
    Group(Vec<Token>),
}

/// `ID_START` in the library: `/^[$_\p{ID_Start}]$/u`.
fn is_id_start(c: char) -> bool {
    in_ranges(id_tables::ID_START, c)
}

/// `ID_CONTINUE` in the library: `/^[$\u200c\u200d\p{ID_Continue}]$/u`.
fn is_id_continue(c: char) -> bool {
    in_ranges(id_tables::ID_CONTINUE, c)
}

/// Whether `c` lies in one of the sorted, disjoint inclusive `ranges`.
fn in_ranges(ranges: &[(u32, u32)], c: char) -> bool {
    let cp = u32::from(c);
    ranges
        .binary_search_by(|(lo, hi)| {
            if *hi < cp {
                std::cmp::Ordering::Less
            } else if *lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

struct Parser<'a> {
    chars: Vec<char>,
    index: usize,
    original: &'a str,
}

fn parse(path: &str) -> Result<Vec<Token>, PathError> {
    let mut parser = Parser {
        chars: path.chars().collect(),
        index: 0,
        original: path,
    };
    parser.consume_until(None)
}

impl Parser<'_> {
    fn error(&self, message: String) -> PathError {
        PathError::new(message, self.original)
    }

    fn consume_until(&mut self, end: Option<char>) -> Result<Vec<Token>, PathError> {
        let mut output = Vec::new();
        let mut path = String::new();
        let write_path = |output: &mut Vec<Token>, path: &mut String| {
            if !path.is_empty() {
                output.push(Token::Text(std::mem::take(path)));
            }
        };
        let len = self.chars.len();
        while self.index < len {
            let value = self.chars[self.index];
            self.index += 1;
            if Some(value) == end {
                write_path(&mut output, &mut path);
                return Ok(output);
            }
            if value == '\\' {
                if self.index == len {
                    return Err(
                        self.error(format!("Unexpected end after \\ at index {}", self.index))
                    );
                }
                path.push(self.chars[self.index]);
                self.index += 1;
                continue;
            }
            if value == ':' || value == '*' {
                let mut name = String::new();
                if self.chars.get(self.index).is_some_and(|c| is_id_start(*c)) {
                    loop {
                        name.push(self.chars[self.index]);
                        self.index += 1;
                        if !self
                            .chars
                            .get(self.index)
                            .is_some_and(|c| is_id_continue(*c))
                        {
                            break;
                        }
                    }
                } else if self.chars.get(self.index) == Some(&'"') {
                    let quote_start = self.index;
                    let mut terminated = false;
                    while self.index < len {
                        self.index += 1;
                        if self.chars.get(self.index) == Some(&'"') {
                            self.index += 1;
                            terminated = true;
                            break;
                        }
                        // Increment over escape characters.
                        if self.chars.get(self.index) == Some(&'\\') {
                            self.index += 1;
                        }
                        // JavaScript appends `undefined` past the end; the quote is then
                        // unterminated, so the name is never used.
                        match self.chars.get(self.index) {
                            Some(c) => name.push(*c),
                            None => name.push_str("undefined"),
                        }
                    }
                    if !terminated {
                        return Err(
                            self.error(format!("Unterminated quote at index {quote_start}"))
                        );
                    }
                }
                if name.is_empty() {
                    return Err(
                        self.error(format!("Missing parameter name at index {}", self.index))
                    );
                }
                write_path(&mut output, &mut path);
                output.push(if value == ':' {
                    Token::Param(name)
                } else {
                    Token::Wildcard(name)
                });
                continue;
            }
            if value == '{' {
                write_path(&mut output, &mut path);
                let tokens = self.consume_until(Some('}'))?;
                output.push(Token::Group(tokens));
                continue;
            }
            if matches!(value, '}' | '(' | ')' | '[' | ']' | '+' | '?' | '!') {
                return Err(self.error(format!("Unexpected {value} at index {}", self.index - 1)));
            }
            path.push(value);
        }
        if let Some(end) = end {
            return Err(self.error(format!(
                "Unexpected end at index {}, expected {end}",
                self.index
            )));
        }
        write_path(&mut output, &mut path);
        Ok(output)
    }
}

// ---------------------------------------------------------------------------------------
// Expansion of optional groups (`flatten`) and the regular expression builder.

#[derive(Debug, Clone)]
enum Flat {
    Text(String),
    Param(String),
    Wildcard(String),
}

fn flatten(
    tokens: &[Token],
    index: usize,
    result: &mut Vec<Flat>,
    callback: &mut dyn FnMut(&mut Vec<Flat>) -> Result<(), PathError>,
) -> Result<(), PathError> {
    let mut index = index;
    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;
        match token {
            Token::Group(inner) => {
                let len = result.len();
                flatten(inner, 0, result, &mut |seq: &mut Vec<Flat>| {
                    flatten(tokens, index, seq, &mut *callback)
                })?;
                result.truncate(len);
                continue;
            }
            Token::Text(value) => result.push(Flat::Text(value.clone())),
            Token::Param(name) => result.push(Flat::Param(name.clone())),
            Token::Wildcard(name) => result.push(Flat::Wildcard(name.clone())),
        }
    }
    callback(result)
}

#[derive(Default)]
struct Builder {
    source: String,
    keys: Vec<PatternKey>,
    alternatives: Vec<Vec<Elem>>,
    combinations: usize,
}

impl Builder {
    fn process(&mut self, path: &str) -> Result<(), PathError> {
        let tokens = parse(path)?;
        let mut result = Vec::new();
        flatten(&tokens, 0, &mut result, &mut |flat: &mut Vec<Flat>| {
            if self.combinations >= MAX_COMBINATIONS {
                return Err(PathError::new("Too many path combinations", path));
            }
            if self.combinations > 0 {
                self.source.push('|');
            }
            let (source, elems) = to_regexp_source(flat, &mut self.keys, path)?;
            self.source.push_str(&source);
            self.alternatives.push(elems);
            self.combinations += 1;
            Ok(())
        })
    }
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `escape` in the library: escapes the regular expression syntax characters.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '+'
                | '*'
                | '?'
                | '^'
                | '$'
                | '{'
                | '}'
                | '('
                | ')'
                | '['
                | ']'
                | '|'
                | '/'
                | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `negate(a, b)` in the library: one code unit at which neither `a` nor `b` starts.
fn negate(a: &str, b: &str) -> (String, UnitClass) {
    if utf16_len(b) > utf16_len(a) {
        return negate(b, a);
    }
    let b = if a == b { "" } else { b };
    if utf16_len(b) > 1 {
        return (
            format!("(?:(?!{}|{})[^])", escape(a), escape(b)),
            UnitClass::new(&[a, b], ""),
        );
    }
    if utf16_len(a) > 1 {
        return (
            format!("(?:(?!{})[^{}])", escape(a), escape(b)),
            UnitClass::new(&[a], b),
        );
    }
    let both = format!("{a}{b}");
    (format!("[^{}]", escape(&both)), UnitClass::new(&[], &both))
}

fn has_in_segment(tokens: &[Flat], mut index: usize, wildcard: bool) -> bool {
    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;
        match token {
            Flat::Wildcard(_) if wildcard => return true,
            Flat::Param(_) if !wildcard => return true,
            Flat::Text(value) if value.contains(DELIMITER) => break,
            _ => {}
        }
    }
    false
}

fn peek_text(tokens: &[Flat], mut index: usize) -> String {
    let mut result = String::new();
    while let Some(Flat::Text(value)) = tokens.get(index) {
        result.push_str(value);
        index += 1;
    }
    result
}

const PARAM_SEEN: u8 = 1;
const WILDCARD_SEEN: u8 = 2;

fn to_regexp_source(
    tokens: &[Flat],
    keys: &mut Vec<PatternKey>,
    original_path: &str,
) -> Result<(String, Vec<Elem>), PathError> {
    let mut result = String::new();
    let mut elems = Vec::new();
    let mut backtrack = String::new();
    let mut wildcard_backtrack = String::new();
    let mut prev_capture_type: u8 = 0;
    let mut has_segment_capture: u8 = 0;
    let mut index = 0;

    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;
        let (name, kind) = match token {
            Flat::Text(value) => {
                result.push_str(&escape(value));
                elems.push(Elem::Text(canonical_units(value)));
                backtrack.push_str(value);
                if prev_capture_type == WILDCARD_SEEN {
                    wildcard_backtrack.push_str(value);
                }
                if value.contains(DELIMITER) {
                    has_segment_capture = 0;
                }
                continue;
            }
            Flat::Param(name) => (name, ParamKind::Param),
            Flat::Wildcard(name) => (name, ParamKind::Wildcard),
        };
        if prev_capture_type != 0 && backtrack.is_empty() {
            let kind_name = match kind {
                ParamKind::Param => "param",
                ParamKind::Wildcard => "wildcard",
            };
            return Err(PathError::new(
                format!("Missing text before \"{name}\" {kind_name}"),
                original_path,
            ));
        }
        let alternatives = match kind {
            ParamKind::Param => {
                let alternatives = if has_segment_capture & WILDCARD_SEEN != 0 {
                    // Seen wildcard in segment.
                    let (neg, class) = negate(DELIMITER, &backtrack);
                    result.push_str(&format!("({neg}+)"));
                    vec![CaptureAlt::Repeat(class)]
                } else if has_in_segment(tokens, index, true) {
                    // See wildcard later in segment.
                    let (neg, class) = negate(DELIMITER, &peek_text(tokens, index));
                    result.push_str(&format!("({neg}+)"));
                    vec![CaptureAlt::Repeat(class)]
                } else if has_segment_capture & PARAM_SEEN != 0 {
                    // Seen parameter in segment.
                    let (neg, class) = negate(DELIMITER, &backtrack);
                    result.push_str(&format!("({neg}+|{})", escape(&backtrack)));
                    vec![
                        CaptureAlt::Repeat(class),
                        CaptureAlt::Text(canonical_units(&backtrack)),
                    ]
                } else {
                    let (neg, class) = negate(DELIMITER, "");
                    result.push_str(&format!("({neg}+)"));
                    vec![CaptureAlt::Repeat(class)]
                };
                prev_capture_type = PARAM_SEEN;
                has_segment_capture |= PARAM_SEEN;
                alternatives
            }
            ParamKind::Wildcard => {
                let alternatives = if has_segment_capture & WILDCARD_SEEN != 0 {
                    // Seen wildcard in segment.
                    let (neg, class) = negate(&backtrack, "");
                    result.push_str(&format!("({neg}+)"));
                    vec![CaptureAlt::Repeat(class)]
                } else if !wildcard_backtrack.is_empty() {
                    // No capture in segment, seen wildcard in path.
                    let (first, first_class) = negate(&wildcard_backtrack, "");
                    let (second, second_class) = negate(DELIMITER, "");
                    result.push_str(&format!("({first}+|{second}+)"));
                    vec![
                        CaptureAlt::Repeat(first_class),
                        CaptureAlt::Repeat(second_class),
                    ]
                } else {
                    result.push_str("([^]+)");
                    vec![CaptureAlt::Repeat(UnitClass::new(&[], ""))]
                };
                wildcard_backtrack.clear();
                prev_capture_type = WILDCARD_SEEN;
                has_segment_capture |= WILDCARD_SEEN;
                alternatives
            }
        };
        keys.push(PatternKey {
            name: name.clone(),
            kind,
        });
        elems.push(Elem::Capture {
            group: keys.len() - 1,
            alternatives,
        });
        backtrack.clear();
    }
    Ok((result, elems))
}

// ---------------------------------------------------------------------------------------
// The matcher.

/// One element of a compiled alternative.
#[derive(Debug, Clone)]
enum Elem {
    /// Literal text, as canonicalized code units.
    Text(Vec<u16>),
    /// A capture group `(X+|...)`.
    Capture {
        group: usize,
        alternatives: Vec<CaptureAlt>,
    },
}

#[derive(Debug, Clone)]
enum CaptureAlt {
    /// `X+`, greedy.
    Repeat(UnitClass),
    /// Literal text.
    Text(Vec<u16>),
}

/// One code unit at which none of `not_starting` starts and which is none of `not_in`:
/// the regular expressions `[^ab]`, `(?:(?!abc)[^d])` and `(?:(?!abc|de)[^])`.
#[derive(Debug, Clone)]
struct UnitClass {
    not_starting: Vec<Vec<u16>>,
    not_in: Vec<u16>,
}

impl UnitClass {
    fn new(not_starting: &[&str], not_in: &str) -> Self {
        Self {
            not_starting: not_starting.iter().map(|s| canonical_units(s)).collect(),
            not_in: canonical_units(not_in),
        }
    }

    fn matches_at(&self, input: &[u16], pos: usize) -> bool {
        let Some(unit) = input.get(pos) else {
            return false;
        };
        !self.not_in.contains(unit)
            && !self
                .not_starting
                .iter()
                .any(|text| input[pos..].starts_with(text))
    }
}

/// The input as UTF-16 code units, both as given (for captures) and canonicalized (for
/// comparisons).
struct Units {
    original: Vec<u16>,
    canonical: Vec<u16>,
}

impl Units {
    fn new(input: &str) -> Self {
        let original: Vec<u16> = input.encode_utf16().collect();
        let canonical = original.iter().map(|u| canonicalize(*u)).collect();
        Self {
            original,
            canonical,
        }
    }
}

fn canonical_units(s: &str) -> Vec<u16> {
    s.encode_utf16().map(canonicalize).collect()
}

/// ECMAScript `Canonicalize(rer, ch)` for a case-insensitive, non-Unicode regular
/// expression: the upper-case mapping of a code unit, unless that mapping is not a single
/// code unit or maps a non-ASCII unit to ASCII.
fn canonicalize(unit: u16) -> u16 {
    if unit < 0x80 {
        return u16::from(u8::try_from(unit).unwrap_or(0).to_ascii_uppercase());
    }
    let Some(c) = char::from_u32(u32::from(unit)) else {
        // A lone surrogate.
        return unit;
    };
    let mut upper = c.to_uppercase();
    let (Some(first), None) = (upper.next(), upper.next()) else {
        return unit;
    };
    match u16::try_from(u32::from(first)) {
        Ok(mapped) if mapped >= 0x80 => mapped,
        _ => unit,
    }
}

/// The capture elements already known not to match at a position.
///
/// Whether `elems[index..]` matches at `pos` does not depend on how the matcher got there
/// (the expressions have no backreferences), so a failure found once is final. Remembering
/// failures keeps the backtracking order, and so the captures, of a JavaScript engine while
/// bounding the work by the number of (element, position) pairs instead of the number of
/// paths through them, which grows exponentially with repeated wildcards (FUZZ-03). The set
/// is only allocated once a capture fails.
#[derive(Default)]
struct Failures(Option<std::collections::HashSet<(usize, usize)>>);

impl Failures {
    fn contains(&self, index: usize, pos: usize) -> bool {
        self.0
            .as_ref()
            .is_some_and(|set| set.contains(&(index, pos)))
    }

    fn insert(&mut self, index: usize, pos: usize) {
        self.0.get_or_insert_default().insert((index, pos));
    }
}

/// Matches `elems[index..]` and then the anchored trailing `(?:\/$)?$` at `pos`, trying
/// alternatives in order and repetitions longest first, as a backtracking JavaScript
/// regular expression engine does.
fn match_elems(
    elems: &[Elem],
    index: usize,
    pos: usize,
    units: &Units,
    caps: &mut [Option<(usize, usize)>],
    failures: &mut Failures,
) -> bool {
    let input = &units.canonical;
    let Some(elem) = elems.get(index) else {
        // `(?:\/$)?$`
        return pos == input.len() || (pos + 1 == input.len() && input[pos] == u16::from(b'/'));
    };
    match elem {
        Elem::Text(text) => {
            input[pos..].starts_with(text)
                && match_elems(elems, index + 1, pos + text.len(), units, caps, failures)
        }
        Elem::Capture {
            group,
            alternatives,
        } => {
            if failures.contains(index, pos) {
                return false;
            }
            for alternative in alternatives {
                match alternative {
                    CaptureAlt::Repeat(class) => {
                        let mut run = 0;
                        while class.matches_at(input, pos + run) {
                            run += 1;
                        }
                        for len in (1..=run).rev() {
                            caps[*group] = Some((pos, pos + len));
                            if match_elems(elems, index + 1, pos + len, units, caps, failures) {
                                return true;
                            }
                        }
                    }
                    CaptureAlt::Text(text) => {
                        if input[pos..].starts_with(text) {
                            caps[*group] = Some((pos, pos + text.len()));
                            if match_elems(
                                elems,
                                index + 1,
                                pos + text.len(),
                                units,
                                caps,
                                failures,
                            ) {
                                return true;
                            }
                        }
                    }
                }
            }
            caps[*group] = None;
            failures.insert(index, pos);
            false
        }
    }
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

#[cfg(test)]
mod tests;
