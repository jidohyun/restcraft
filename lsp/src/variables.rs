//! Stage 2: `{{...}}` substitution on raw block text.
//!
//! Resolution order mirrors vscode-restclient `utils/variableProcessor.ts`:
//! system -> request (TODO, Phase 2) -> file -> environment.
//!
//! Like the original, a reference that cannot be resolved is left in the
//! output verbatim; the reasons are collected in `Substitution::errors` so
//! the caller can surface diagnostics before/instead of sending.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use chrono::{DateTime, Local, Months, SecondsFormat, TimeDelta, TimeZone, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use rand::Rng;
use thiserror::Error;
use uuid::Uuid;

use crate::parser::FileVariable;
use crate::settings::SHARED_ENV_KEY;

#[derive(Debug, Error)]
pub enum VariableError {
    #[error("undefined variable: {0}")]
    Undefined(String),
    #[error("invalid system variable: {0}")]
    InvalidSystemVariable(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Everything needed to resolve `{{...}}` references for one send.
pub struct VariableContext<'a> {
    /// File variables in declaration order; values may themselves contain refs.
    pub file_variables: &'a [FileVariable],
    /// Already merged environment (`$shared` + selected env, selected wins) —
    /// see `settings::resolve_environment`.
    pub environment: &'a HashMap<String, String>,
    /// Directory of the .http file; `{{$dotenv ...}}` searches `.env` upward
    /// from here.
    pub document_dir: &'a Path,
}

/// Result of one substitution pass. References that failed to resolve keep
/// their original `{{...}}` text; the reasons land in `errors` (diagnostics).
#[derive(Debug, Default)]
pub struct Substitution {
    pub text: String,
    pub errors: Vec<VariableError>,
}

/// Replaces every `{{...}}` occurrence in `text`.
///
/// vscode-restclient semantics: references are single-line (`.` in
/// `/\{{2}(.+?)\}{2}/` does not cross newlines), provider order is
/// system -> file -> environment, and unresolvable references stay verbatim.
/// File/environment results are stable within one call (the original caches
/// them); system variables resolve fresh per occurrence (two `{{$guid}}`
/// differ).
pub fn substitute(text: &str, ctx: &VariableContext) -> Substitution {
    let mut resolver = Resolver {
        ctx,
        file_values: file_value_map(ctx.file_variables),
        memo: HashMap::new(),
        visiting: Vec::new(),
        errors: Vec::new(),
    };
    let text = resolver.substitute_text(text);
    Substitution {
        text,
        errors: resolver.errors,
    }
}

/// Resolves one `$...` system variable expression (without braces):
/// - `$guid`
/// - `$randomInt min max`
/// - `$timestamp [offset unit]`
/// - `$datetime rfc1123|iso8601|"custom format" [offset unit]`
/// - `$localDatetime rfc1123|iso8601|"custom format" [offset unit]`
/// - `$processEnv [%]name`  (`%` = indirect via file/env variable)
/// - `$dotenv [%]name`
///
/// Returns `None` when `expr` is not a known system variable, so callers can
/// fall through to file/environment lookup. `$aadToken`/`$oidcAccessToken`/
/// `$aadV2Token` are intentionally unknown (out of MVP scope).
pub fn resolve_system_variable(
    expr: &str,
    ctx: &VariableContext,
) -> Option<Result<String, VariableError>> {
    let name = expr.split_whitespace().next()?;
    let rest = &expr[name.len()..];
    match name {
        "$guid" => Some(Ok(Uuid::new_v4().to_string())),
        "$randomInt" => Some(resolve_random_int(expr)),
        "$timestamp" => Some(resolve_timestamp(rest)),
        "$datetime" => Some(resolve_datetime(rest, "$datetime", false)),
        "$localDatetime" => Some(resolve_datetime(rest, "$localDatetime", true)),
        "$processEnv" => Some(resolve_process_env(rest, ctx)),
        "$dotenv" => Some(resolve_dotenv(rest, ctx)),
        _ => None,
    }
}

// TODO(Phase 2): request variable chaining — {{name.response.body.$.json.path}}
// (vscode-restclient utils/requestVariableCache.ts + requestVariableProvider.ts)

struct Resolver<'a> {
    ctx: &'a VariableContext<'a>,
    /// name -> escape-processed raw value; last definition wins, like the
    /// original `FileVariableProvider` document scan.
    file_values: HashMap<String, String>,
    /// Fully resolved file variables (the original resolves the file variable
    /// map once per request, so repeated refs share one value).
    memo: HashMap<String, String>,
    /// File variables currently being resolved — cycle detection.
    visiting: Vec<String>,
    errors: Vec<VariableError>,
}

impl Resolver<'_> {
    fn substitute_text(&mut self, text: &str) -> String {
        replace_references(text, |_, name| self.resolve_reference(name))
    }

    /// `None` = leave the original `{{...}}` in place (error already pushed,
    /// except for non-reference quirks the original also leaves alone).
    fn resolve_reference(&mut self, name: &str) -> Option<String> {
        if let Some(result) = resolve_system_variable(name, self.ctx) {
            match result {
                Ok(value) => return Some(value),
                Err(error) => {
                    self.errors.push(error);
                    return None;
                }
            }
        }

        // TODO(Phase 2): request variables resolve here, before file variables.

        // `{{%name}}` URL-encodes a *file* variable (original FileVariableProvider).
        let (file_name, url_encode) = match name.strip_prefix('%') {
            Some(stripped) => (stripped, true),
            None => (name, false),
        };
        if self.file_values.contains_key(file_name) {
            let value = self.resolve_file_variable(file_name)?;
            return Some(if url_encode {
                encode_uri_component(&value)
            } else {
                value
            });
        }

        if let Some(value) = self.ctx.environment.get(name) {
            // Env values may carry `{{$shared key}}` refs that the merge step
            // (settings.rs) did not expand — mirror mapEnvironmentVariables.
            let value = value.clone();
            return Some(expand_shared_refs(&value, self.ctx.environment));
        }

        // `{{$shared name}}` written directly in the request text.
        if let Some(key) = shared_key(name) {
            if let Some(value) = self.ctx.environment.get(key) {
                return Some(value.clone());
            }
        }

        self.errors.push(VariableError::Undefined(name.to_string()));
        None
    }

    fn resolve_file_variable(&mut self, name: &str) -> Option<String> {
        if let Some(value) = self.memo.get(name) {
            return Some(value.clone());
        }
        if self.visiting.iter().any(|n| n == name) {
            self.errors.push(VariableError::Other(anyhow!(
                "circular file variable reference: {name} (chain: {})",
                self.visiting.join(" -> ")
            )));
            return None;
        }
        let raw = self.file_values.get(name)?.clone();
        self.visiting.push(name.to_string());
        let resolved = self.substitute_text(&raw);
        self.visiting.pop();
        self.memo.insert(name.to_string(), resolved.clone());
        Some(resolved)
    }
}

/// Scans `text` for `{{...}}` references (single-line, shortest match — JS
/// `/\{{2}(.+?)\}{2}/g` semantics) and replaces each with `resolve(raw, name)`,
/// keeping `raw` when the resolver returns `None`.
fn replace_references(
    text: &str,
    mut resolve: impl FnMut(&str, &str) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(rel) = text[i..].find("{{") {
        let open = i + rel;
        out.push_str(&text[i..open]);
        let close = match reference_close(text, open) {
            Some(close) => close,
            None => {
                out.push_str("{{");
                i = open + 2;
                continue;
            }
        };
        let raw = &text[open..close + 2];
        let name = text[open + 2..close].trim();
        if name.is_empty() {
            out.push_str("{{");
            i = open + 2;
            continue;
        }
        match resolve(raw, name) {
            Some(value) => out.push_str(&value),
            None => out.push_str(raw),
        }
        i = close + 2;
    }
    out.push_str(&text[i..]);
    out
}

/// Index of the `}}` closing the reference opened at `open`, or `None` when
/// there is none on the same line (`.` does not match newlines in JS).
fn reference_close(text: &str, open: usize) -> Option<usize> {
    let inner_start = open + 2;
    let close = inner_start + text[inner_start..].find("}}")?;
    if text[inner_start..close].contains(['\n', '\r']) {
        return None;
    }
    Some(close)
}

/// `$shared name` -> `name` (must be separated by whitespace).
fn shared_key(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(SHARED_ENV_KEY)?;
    let key = rest.trim();
    (rest.starts_with(|c: char| c.is_whitespace()) && !key.is_empty()).then_some(key)
}

/// Expands `{{$shared key}}` references inside an environment value
/// (vscode-restclient does this while merging environments). Anything else
/// stays verbatim — the original never re-scans substituted env values.
fn expand_shared_refs(value: &str, environment: &HashMap<String, String>) -> String {
    if !value.contains("{{") {
        return value.to_string();
    }
    replace_references(value, |_, name| {
        shared_key(name).and_then(|key| environment.get(key).cloned())
    })
}

// ---------------------------------------------------------------------------
// File variables
// ---------------------------------------------------------------------------

fn file_value_map(variables: &[FileVariable]) -> HashMap<String, String> {
    let mut map = HashMap::with_capacity(variables.len());
    for variable in variables {
        // Later definitions win, matching the original document scan.
        map.insert(variable.name.clone(), unescape_file_value(&variable.value));
    }
    map
}

/// `\n`/`\r`/`\t` escapes in file variable values; any other escaped char is
/// kept as-is with the backslash dropped (original FileVariableProvider).
fn unescape_file_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {} // trailing backslash is consumed, like the original
        }
    }
    out
}

/// JS `encodeURIComponent`: everything except `A-Za-z0-9 - _ . ! ~ * ' ( )`.
const ENCODE_URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

fn encode_uri_component(value: &str) -> String {
    utf8_percent_encode(value, ENCODE_URI_COMPONENT).to_string()
}

// ---------------------------------------------------------------------------
// System variables
// ---------------------------------------------------------------------------

fn resolve_random_int(expr: &str) -> Result<String, VariableError> {
    let mut tokens = expr.split_whitespace().skip(1);
    let range = (|| {
        let min: i64 = tokens.next()?.parse().ok()?;
        let max: i64 = tokens.next()?.parse().ok()?;
        (min < max).then_some(min..max)
    })();
    match range {
        Some(range) => Ok(rand::rng().random_range(range).to_string()),
        None => Err(VariableError::InvalidSystemVariable(format!(
            "$randomInt requires two integers with min < max: `{expr}`"
        ))),
    }
}

fn resolve_timestamp(rest: &str) -> Result<String, VariableError> {
    let now = Utc::now();
    // Like the original regex, a malformed offset is ignored (treated as now).
    let date = match parse_offset(rest) {
        Some((offset, unit)) => apply_offset(now, offset, unit).ok_or_else(|| {
            VariableError::InvalidSystemVariable(format!("$timestamp offset out of range:{rest}"))
        })?,
        None => now,
    };
    Ok(date.timestamp().to_string())
}

enum FormatKind {
    Rfc1123,
    Iso8601,
    Custom(String),
}

fn resolve_datetime(rest: &str, label: &str, local: bool) -> Result<String, VariableError> {
    let (kind, remainder) = parse_format_spec(rest, label)?;
    let offset = parse_offset(remainder);
    let out_of_range = || {
        VariableError::InvalidSystemVariable(format!("{label} offset out of range:{remainder}"))
    };

    if local {
        let mut date = Local::now();
        if let Some((n, unit)) = offset {
            date = apply_offset(date, n, unit).ok_or_else(out_of_range)?;
        }
        Ok(match kind {
            // dayjs 'ddd, DD MMM YYYY HH:mm:ss ZZ' (en locale)
            FormatKind::Rfc1123 => date.format("%a, %d %b %Y %H:%M:%S %z").to_string(),
            // dayjs .format() default: no millis, offset with colon
            FormatKind::Iso8601 => date.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            FormatKind::Custom(fmt) => date.format(&dayjs_to_chrono(&fmt, label)?).to_string(),
        })
    } else {
        let mut date = Utc::now();
        if let Some((n, unit)) = offset {
            date = apply_offset(date, n, unit).ok_or_else(out_of_range)?;
        }
        Ok(match kind {
            // JS Date#toUTCString()
            FormatKind::Rfc1123 => date.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
            // JS Date#toISOString(): millisecond precision + literal Z
            FormatKind::Iso8601 => date.to_rfc3339_opts(SecondsFormat::Millis, true),
            FormatKind::Custom(fmt) => date.format(&dayjs_to_chrono(&fmt, label)?).to_string(),
        })
    }
}

fn parse_format_spec<'a>(
    rest: &'a str,
    label: &str,
) -> Result<(FormatKind, &'a str), VariableError> {
    let rest = rest.trim_start();
    if let Some(remainder) = rest.strip_prefix("rfc1123") {
        return Ok((FormatKind::Rfc1123, remainder));
    }
    if let Some(remainder) = rest.strip_prefix("iso8601") {
        return Ok((FormatKind::Iso8601, remainder));
    }
    if let Some(quote) = rest.chars().next().filter(|c| matches!(c, '\'' | '"')) {
        // Greedy `('.+'|".+")` in the original: the closing quote is the last
        // occurrence of the same quote char.
        if let Some(close) = rest.rfind(quote).filter(|&i| i > 1) {
            return Ok((
                FormatKind::Custom(rest[1..close].to_string()),
                &rest[close + 1..],
            ));
        }
    }
    Err(VariableError::InvalidSystemVariable(format!(
        "{label} requires a format: rfc1123 | iso8601 | quoted custom format"
    )))
}

/// Translates a Day.js format string to a chrono one. Only the major tokens
/// (`YYYY` `MM` `DD` `HH` `mm` `ss`) are supported; any other alphabetic
/// token is an error (MVP scope).
fn dayjs_to_chrono(fmt: &str, label: &str) -> Result<String, VariableError> {
    const TOKENS: [(&str, &str); 6] = [
        ("YYYY", "%Y"),
        ("MM", "%m"),
        ("DD", "%d"),
        ("HH", "%H"),
        ("mm", "%M"),
        ("ss", "%S"),
    ];
    let mut out = String::with_capacity(fmt.len() * 2);
    let mut rest = fmt;
    'outer: while !rest.is_empty() {
        for (token, replacement) in TOKENS {
            if let Some(after) = rest.strip_prefix(token) {
                out.push_str(replacement);
                rest = after;
                continue 'outer;
            }
        }
        let c = rest.chars().next().expect("rest is non-empty");
        if c.is_ascii_alphabetic() {
            return Err(VariableError::InvalidSystemVariable(format!(
                "{label}: unsupported format token at `{rest}` \
                 (supported: YYYY MM DD HH mm ss)"
            )));
        }
        if c == '%' {
            out.push_str("%%");
        } else {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    Ok(out)
}

/// `-?\d+ (y|Q|M|w|d|h|m|s|ms)` after a system variable name. Anything else
/// yields `None` — the original's optional regex group silently ignores it.
fn parse_offset(rest: &str) -> Option<(i64, &str)> {
    const UNITS: [&str; 9] = ["y", "Q", "M", "w", "d", "h", "m", "s", "ms"];
    let mut tokens = rest.split_whitespace();
    let offset = tokens.next()?.parse().ok()?;
    let unit = tokens.next()?;
    UNITS.contains(&unit).then_some((offset, unit))
}

fn apply_offset<Tz: TimeZone>(date: DateTime<Tz>, offset: i64, unit: &str) -> Option<DateTime<Tz>> {
    match unit {
        // dayjs adds calendar months for y/Q/M (clamping the day), not fixed
        // durations — chrono::Months matches.
        "y" | "Q" | "M" => {
            let factor = match unit {
                "y" => 12,
                "Q" => 3,
                _ => 1,
            };
            let n = offset.checked_mul(factor)?;
            let months = Months::new(u32::try_from(n.unsigned_abs()).ok()?);
            if n >= 0 {
                date.checked_add_months(months)
            } else {
                date.checked_sub_months(months)
            }
        }
        "w" => date.checked_add_signed(TimeDelta::try_weeks(offset)?),
        "d" => date.checked_add_signed(TimeDelta::try_days(offset)?),
        "h" => date.checked_add_signed(TimeDelta::try_hours(offset)?),
        "m" => date.checked_add_signed(TimeDelta::try_minutes(offset)?),
        "s" => date.checked_add_signed(TimeDelta::try_seconds(offset)?),
        "ms" => date.checked_add_signed(TimeDelta::try_milliseconds(offset)?),
        _ => None,
    }
}

fn resolve_process_env(rest: &str, ctx: &VariableContext) -> Result<String, VariableError> {
    let (name, indirect) = parse_env_ref(rest, "$processEnv")?;
    let name = if indirect {
        resolve_indirect_name(&name, ctx)
    } else {
        name
    };
    // Missing process env var resolves to "" in the original, not an error.
    Ok(std::env::var(name).unwrap_or_default())
}

fn resolve_dotenv(rest: &str, ctx: &VariableContext) -> Result<String, VariableError> {
    let (name, indirect) = parse_env_ref(rest, "$dotenv")?;
    let path = find_dotenv(ctx.document_dir).ok_or_else(|| {
        anyhow!(
            "no .env file found searching upward from {}",
            ctx.document_dir.display()
        )
    })?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("failed to read {}: {e}", path.display()))?;
    let values = parse_dotenv(&content);
    let key = if indirect {
        resolve_indirect_name(&name, ctx)
    } else {
        name
    };
    values
        .get(&key)
        .cloned()
        .ok_or_else(|| VariableError::Undefined(format!("$dotenv {key}")))
}

/// `[%]name` argument of `$processEnv`/`$dotenv`.
fn parse_env_ref(rest: &str, label: &str) -> Result<(String, bool), VariableError> {
    let invalid = || {
        VariableError::InvalidSystemVariable(format!("{label} requires a variable name:{rest}"))
    };
    let token = rest.split_whitespace().next().ok_or_else(invalid)?;
    let (name, indirect) = match token.strip_prefix('%') {
        Some(stripped) => (stripped, true),
        None => (token, false),
    };
    if name.is_empty() {
        return Err(invalid());
    }
    Ok((name.to_string(), indirect))
}

/// `%name` indirection: the actual env var name is itself stored in an
/// environment or file variable. Unresolvable -> the name itself, like the
/// original `resolveSettingsEnvironmentVariable`.
fn resolve_indirect_name(name: &str, ctx: &VariableContext) -> String {
    if let Some(value) = ctx.environment.get(name) {
        return value.clone();
    }
    if let Some(variable) = ctx.file_variables.iter().rev().find(|v| v.name == name) {
        return unescape_file_value(&variable.value);
    }
    name.to_string()
}

fn find_dotenv(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        let candidate = dir.join(".env");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

/// Minimal dotenv parser: `KEY=value` lines, `#` comments, optional `export `
/// prefix, surrounding quotes stripped, `\n` unescaped inside double quotes.
fn parse_dotenv(content: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = match value.chars().next() {
            Some(q @ ('"' | '\'')) if value.len() >= 2 && value.ends_with(q) => {
                let inner = &value[1..value.len() - 1];
                if q == '"' {
                    inner.replace("\\n", "\n")
                } else {
                    inner.to_string()
                }
            }
            _ => value.to_string(),
        };
        values.insert(key.to_string(), value);
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("restcraft-vars-{}", Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fv(name: &str, value: &str) -> FileVariable {
        FileVariable {
            name: name.to_string(),
            value: value.to_string(),
            line: 0,
        }
    }

    fn ctx<'a>(
        file_variables: &'a [FileVariable],
        environment: &'a HashMap<String, String>,
        document_dir: &'a Path,
    ) -> VariableContext<'a> {
        VariableContext {
            file_variables,
            environment,
            document_dir,
        }
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn assert_uuid(s: &str) {
        assert_eq!(s.len(), 36, "not a uuid: {s}");
        for (i, c) in s.char_indices() {
            match i {
                8 | 13 | 18 | 23 => assert_eq!(c, '-', "not a uuid: {s}"),
                14 => assert_eq!(c, '4', "not a v4 uuid: {s}"),
                _ => assert!(c.is_ascii_hexdigit(), "not a uuid: {s}"),
            }
        }
    }

    #[test]
    fn guid_resolves_fresh_per_occurrence() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{$guid}}|{{$guid}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let (a, b) = s.text.split_once('|').unwrap();
        assert_uuid(a);
        assert_uuid(b);
        assert_ne!(a, b, "system variables must not be cached per name");
    }

    #[test]
    fn random_int_within_half_open_range() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        for _ in 0..50 {
            let s = substitute("{{$randomInt -3 4}}", &c);
            assert!(s.errors.is_empty(), "{:?}", s.errors);
            let n: i64 = s.text.parse().unwrap();
            assert!((-3..4).contains(&n), "out of range: {n}");
        }
    }

    #[test]
    fn random_int_rejects_min_not_less_than_max() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{$randomInt 10 5}}", &c);
        assert_eq!(s.text, "{{$randomInt 10 5}}");
        assert!(matches!(
            s.errors.as_slice(),
            [VariableError::InvalidSystemVariable(_)]
        ));
    }

    #[test]
    fn timestamp_now_and_with_offset() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let now = Utc::now().timestamp();

        let s = substitute("{{$timestamp}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let ts: i64 = s.text.parse().unwrap();
        assert!((ts - now).abs() <= 5, "ts={ts} now={now}");

        let s = substitute("{{$timestamp -1 d}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let ts: i64 = s.text.parse().unwrap();
        assert!((ts - (now - 86_400)).abs() <= 5, "ts={ts} now={now}");
    }

    #[test]
    fn datetime_rfc1123() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{$datetime rfc1123}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert!(s.text.ends_with(" GMT"), "{}", s.text);
        DateTime::parse_from_rfc2822(&s.text).unwrap();
    }

    #[test]
    fn datetime_iso8601() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{$datetime iso8601}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert!(s.text.ends_with('Z'), "{}", s.text);
        DateTime::parse_from_rfc3339(&s.text).unwrap();
    }

    #[test]
    fn datetime_custom_format_with_offset() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{$datetime \"YYYY-MM-DD\" 1 y}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let date = chrono::NaiveDate::parse_from_str(&s.text, "%Y-%m-%d").unwrap();
        let expected = Utc::now().checked_add_months(Months::new(12)).unwrap();
        assert_eq!(date.year(), expected.year());
    }

    #[test]
    fn datetime_unsupported_token_is_error() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{$datetime 'YYYY-MMM'}}", &c);
        assert_eq!(s.text, "{{$datetime 'YYYY-MMM'}}");
        assert!(matches!(
            s.errors.as_slice(),
            [VariableError::InvalidSystemVariable(_)]
        ));

        // missing format entirely is also an error (regex requires one)
        let s = substitute("{{$datetime}}", &c);
        assert_eq!(s.text, "{{$datetime}}");
        assert_eq!(s.errors.len(), 1);
    }

    #[test]
    fn local_datetime_formats() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{$localDatetime iso8601}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        DateTime::parse_from_rfc3339(&s.text).unwrap();

        let s = substitute("{{$localDatetime rfc1123}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        DateTime::parse_from_str(&s.text, "%a, %d %b %Y %H:%M:%S %z").unwrap();
    }

    #[test]
    fn process_env_direct_and_missing() {
        std::env::set_var("RESTCRAFT_TEST_DIRECT", "v1");
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{$processEnv RESTCRAFT_TEST_DIRECT}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "v1");

        // upstream resolves a missing process env var to "" without error
        let s = substitute("{{$processEnv RESTCRAFT_TEST_SURELY_MISSING}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "");
    }

    #[test]
    fn process_env_indirect_through_environment() {
        std::env::set_var("RESTCRAFT_TEST_INDIRECT", "v2");
        let environment = env(&[("envVarName", "RESTCRAFT_TEST_INDIRECT")]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{$processEnv %envVarName}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "v2");
    }

    #[test]
    fn dotenv_upward_search_and_indirect() {
        let tmp = TempDir::new();
        fs::write(
            tmp.0.join(".env"),
            "# comment\nexport KEY=plain\nQUOTED=\"a b\"\n",
        )
        .unwrap();
        let nested = tmp.0.join("a/b");
        fs::create_dir_all(&nested).unwrap();

        let vars = [fv("ref", "QUOTED")];
        let environment = env(&[]);
        let c = ctx(&vars, &environment, &nested);
        let s = substitute("{{$dotenv KEY}}|{{$dotenv %ref}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "plain|a b");

        let s = substitute("{{$dotenv NOPE}}", &c);
        assert_eq!(s.text, "{{$dotenv NOPE}}");
        assert!(matches!(s.errors.as_slice(), [VariableError::Undefined(_)]));
    }

    #[test]
    fn file_variable_simple() {
        let vars = [fv("host", "example.com")];
        let environment = env(&[]);
        let c = ctx(&vars, &environment, Path::new("."));
        let s = substitute("GET https://{{host}}/api", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "GET https://example.com/api");
    }

    #[test]
    fn file_variable_nested_references() {
        let vars = [
            fv("base", "https://example.com"),
            fv("url", "{{base}}/api"),
        ];
        let environment = env(&[]);
        let c = ctx(&vars, &environment, Path::new("."));
        let s = substitute("GET {{url}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "GET https://example.com/api");
    }

    #[test]
    fn file_variable_cycle_detected() {
        let vars = [fv("a", "{{b}}"), fv("b", "{{a}}")];
        let environment = env(&[]);
        let c = ctx(&vars, &environment, Path::new("."));
        let s = substitute("{{a}}", &c);
        assert!(!s.errors.is_empty(), "cycle must be reported");
        assert!(s.text.contains("{{"), "unresolved literal must remain: {}", s.text);
    }

    #[test]
    fn file_variable_escape_sequences() {
        let vars = [fv("v", r"line1\nline2\tend\\x")];
        let environment = env(&[]);
        let c = ctx(&vars, &environment, Path::new("."));
        let s = substitute("{{v}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "line1\nline2\tend\\x");
    }

    #[test]
    fn file_variable_percent_prefix_url_encodes() {
        let vars = [fv("q", "a b&c=d")];
        let environment = env(&[]);
        let c = ctx(&vars, &environment, Path::new("."));
        let s = substitute("{{%q}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "a%20b%26c%3Dd");
    }

    #[test]
    fn file_variable_with_system_ref_is_stable_within_one_pass() {
        let vars = [fv("id", "{{$guid}}")];
        let environment = env(&[]);
        let c = ctx(&vars, &environment, Path::new("."));
        let s = substitute("{{id}}|{{id}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let (a, b) = s.text.split_once('|').unwrap();
        assert_uuid(a);
        assert_eq!(a, b, "file variables resolve once per substitution");
    }

    #[test]
    fn undefined_reference_kept_with_error() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("GET {{nope}}/x", &c);
        assert_eq!(s.text, "GET {{nope}}/x");
        assert!(matches!(
            s.errors.as_slice(),
            [VariableError::Undefined(name)] if name == "nope"
        ));
    }

    #[test]
    fn environment_variable_and_shared_reference() {
        let environment = env(&[("token", "abc"), ("host", "example.com")]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{token}}@{{$shared host}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "abc@example.com");
    }

    #[test]
    fn shared_reference_inside_environment_value() {
        let environment = env(&[
            ("base", "https://example.com"),
            ("api", "{{$shared base}}/v1"),
        ]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("GET {{api}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "GET https://example.com/v1");
    }

    #[test]
    fn environment_file_load_and_merge() {
        let tmp = TempDir::new();
        let json = r#"{
            "$shared": { "version": "v1", "host": "shared.example.com" },
            "dev": { "host": "dev.example.com" }
        }"#;
        let path = tmp.0.join(crate::settings::ENV_FILE_NAME);
        fs::write(&path, json).unwrap();

        let raw: HashMap<String, HashMap<String, String>> =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // merge the way settings::resolve_environment will: $shared base,
        // selected environment wins
        let mut environment = raw.get(SHARED_ENV_KEY).cloned().unwrap_or_default();
        environment.extend(raw.get("dev").cloned().unwrap_or_default());

        let c = ctx(&[], &environment, &tmp.0);
        let s = substitute("{{host}}/{{version}}", &c);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.text, "dev.example.com/v1");
    }

    #[test]
    fn resolve_system_variable_falls_through_for_unknown_names() {
        let environment = env(&[]);
        let c = ctx(&[], &environment, Path::new("."));
        assert!(resolve_system_variable("notSystem", &c).is_none());
        // out of MVP scope -> not a system variable -> ends up Undefined
        assert!(resolve_system_variable("$aadToken", &c).is_none());
        assert!(matches!(resolve_system_variable("$guid", &c), Some(Ok(_))));
    }

    #[test]
    fn reference_cannot_span_lines() {
        let environment = env(&[("host", "example.com")]);
        let c = ctx(&[], &environment, Path::new("."));
        let s = substitute("{{ho\nst}}", &c);
        assert_eq!(s.text, "{{ho\nst}}");
        assert!(s.errors.is_empty(), "{:?}", s.errors);
    }
}
