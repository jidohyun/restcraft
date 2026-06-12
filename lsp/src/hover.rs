//! textDocument/hover for `{{...}}` references.
//!
//! Semantics mirror vscode-restclient
//! `providers/environmentOrFileVariableHoverProvider.ts` (file/environment
//! variables show their resolved value) and
//! `providers/requestVariableHoverProvider.ts` (request variable paths show
//! the cached resolution). Two deliberate extensions on top of the original:
//! system variables show their description, and an unsent request variable
//! shows a "has not been sent" notice instead of staying silent.
//!
//! Request variable resolution is injected through
//! [`completion::RequestVariableSource`] — the cache itself lives in
//! `variables::request_vars` and backend.rs wires it in (`CacheSource`).

use std::collections::HashMap;
use std::path::Path;

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range};

use crate::completion::{
    self, RequestVariableResolution, RequestVariableSource, ResolvedValue, SYSTEM_VARIABLES,
};
use crate::parser;
use crate::variables::{self, VariableContext};

/// Hover for the `{{...}}` reference under the cursor, if any.
/// `environment` is the merged `$shared` + selected environment.
pub fn hover(
    text: &str,
    position: Position,
    environment: &HashMap<String, String>,
    document_dir: &Path,
    request_variables: &dyn RequestVariableSource,
) -> Option<Hover> {
    let line_text = completion::line_at(text, position.line);
    let cursor = completion::utf16_to_byte(line_text, position.character);
    let (open, end) = completion::reference_spans(line_text)
        .into_iter()
        .find(|&(open, end)| open <= cursor && cursor <= end)?;
    let inner = &line_text[open + 2..end - 2];
    let range = inner_range(line_text, position.line, open, end);

    // Request variable path first, like the original provider pair: only the
    // `name.<...>` shape is a request variable reference, and only when the
    // name is actually defined by a `# @name` line; otherwise fall through.
    if let Some(name) = request_variable_reference_name(inner) {
        if completion::request_variable_names(text).iter().any(|n| n == name) {
            return request_variable_hover(inner.trim(), name, range, request_variables);
        }
    }

    let name = inner.trim();

    // System variable: description (the original has no hover here at all).
    let first_token = name.split_whitespace().next()?;
    if let Some(system) = SYSTEM_VARIABLES.iter().find(|s| s.name == first_token) {
        return Some(markdown_hover(
            &[system.description, &format!("*System Variable* `{}`", system.name)],
            range,
        ));
    }

    let document = parser::parse_document(text);
    let ctx = VariableContext {
        file_variables: &document.file_variables,
        environment,
        document_dir,
    };
    let reference = &line_text[open..end]; // the exact `{{...}}` text

    // File variable (`{{name}}` / URL-encoding `{{%name}}`).
    let file_name = name.strip_prefix('%').unwrap_or(name);
    if document.file_variables.iter().any(|v| v.name == file_name) {
        return resolved_reference_hover(reference, &ctx, "File Variable", file_name, range);
    }

    // Environment variable.
    if environment.contains_key(name) {
        return resolved_reference_hover(reference, &ctx, "Environment Variable", name, range);
    }

    // `{{$shared key}}` written directly in the request text.
    if let Some(rest) = name.strip_prefix("$shared") {
        let key = rest.trim();
        if rest.starts_with(char::is_whitespace) && environment.contains_key(key) {
            return resolved_reference_hover(reference, &ctx, "Environment Variable", key, range);
        }
    }

    None
}

fn request_variable_hover(
    path: &str,
    name: &str,
    range: Range,
    source: &dyn RequestVariableSource,
) -> Option<Hover> {
    let label = format!("*Request Variable* `{name}`");
    match source.resolve(path) {
        RequestVariableResolution::NotSent => Some(markdown_hover(
            &["*Request variable has not been sent*", &label],
            range,
        )),
        RequestVariableResolution::Resolved(ResolvedValue::Text(value)) => {
            Some(markdown_hover(&[&value, &label], range))
        }
        RequestVariableResolution::Resolved(ResolvedValue::Json(json)) => {
            let block = format!("```json\n{json}\n```");
            Some(markdown_hover(&[&block, &label], range))
        }
        // Mirrors the original: a warning resolution (incorrect header name,
        // JSONPath miss, incomplete path, ...) produces no hover.
        RequestVariableResolution::Warning(_) => None,
    }
}

/// Resolves `reference` (the exact `{{...}}` text) through variables.rs and
/// hovers the value. Unresolvable -> no hover, like the original providers.
fn resolved_reference_hover(
    reference: &str,
    ctx: &VariableContext,
    label: &str,
    name: &str,
    range: Range,
) -> Option<Hover> {
    let substitution = variables::substitute(reference, ctx);
    if !substitution.errors.is_empty() {
        return None;
    }
    Some(markdown_hover(
        &[&substitution.text, &format!("*{label}* `{name}`")],
        range,
    ))
}

/// Port of vscode-restclient `requestVariableReferenceRegex`
/// `/\{{2}(\w+)\.(response|request)?(\.body(\..*?)?|\.headers(\.[\w-]+)?)?\}{2}/`
/// applied to the text between the braces; returns the request name when the
/// whole inner text has the reference shape.
fn request_variable_reference_name(inner: &str) -> Option<&str> {
    let dot = inner.find('.')?;
    let name = &inner[..dot];
    if name.is_empty() || !name.bytes().all(completion::is_word_byte) {
        return None;
    }
    let rest = &inner[dot + 1..];
    // `(response|request)?` — optional; on no match the part must carry its
    // own leading dot (so `name.body` is NOT a reference but `name..body` is,
    // faithfully to the original regex).
    let rest = rest
        .strip_prefix("response")
        .or_else(|| rest.strip_prefix("request"))
        .unwrap_or(rest);
    if rest.is_empty() {
        return Some(name);
    }
    let valid = if let Some(body_path) = rest.strip_prefix(".body") {
        // `(\..*?)?` — empty or a dot followed by anything
        body_path.is_empty() || body_path.starts_with('.')
    } else if let Some(header) = rest.strip_prefix(".headers") {
        // `(\.[\w-]+)?` — empty or a dot followed by 1+ word/hyphen chars
        header.is_empty()
            || header.strip_prefix('.').is_some_and(|h| {
                !h.is_empty() && h.bytes().all(|b| completion::is_word_byte(b) || b == b'-')
            })
    } else {
        false
    };
    valid.then_some(name)
}

/// Range of the text between the braces, in UTF-16 columns.
fn inner_range(line: &str, line_idx: u32, open: usize, end: usize) -> Range {
    Range {
        start: Position {
            line: line_idx,
            character: completion::byte_to_utf16(line, open + 2),
        },
        end: Position {
            line: line_idx,
            character: completion::byte_to_utf16(line, end - 2),
        },
    }
}

fn markdown_hover(sections: &[&str], range: Range) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: sections.join("\n\n"),
        }),
        range: Some(range),
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{NoRequestVariables, RequestEntity};

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn markdown(hover: &Hover) -> &str {
        match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            other => panic!("unexpected hover contents: {other:?}"),
        }
    }

    /// Resolves every path to a fixed value.
    struct Resolves(RequestVariableResolution);

    impl RequestVariableSource for Resolves {
        fn has(&self, _name: &str) -> bool {
            !matches!(self.0, RequestVariableResolution::NotSent)
        }

        fn headers(&self, _name: &str, _entity: RequestEntity) -> Option<Vec<(String, String)>> {
            None
        }

        fn resolve(&self, _path: &str) -> RequestVariableResolution {
            self.0.clone()
        }
    }

    const DOC: &str = "@host = https://example.com\n\
                       # @name login\n\
                       GET {{host}}/api\n\
                       X-Token: {{token}}\n\
                       V: {{$guid}}\n\
                       R: {{login.response.body.$.token}}\n\
                       N: {{nope}}\n\
                       E: {{%host}}";

    fn hover_at(
        line: u32,
        character: u32,
        environment: &HashMap<String, String>,
        source: &dyn RequestVariableSource,
    ) -> Option<Hover> {
        hover(DOC, pos(line, character), environment, Path::new("."), source)
    }

    #[test]
    fn file_variable_shows_resolved_value_and_inner_range() {
        let environment = env(&[]);
        // line 2 `GET {{host}}/api` — `host` spans cols 6..10
        let h = hover_at(2, 7, &environment, &NoRequestVariables).unwrap();
        let md = markdown(&h);
        assert!(md.contains("https://example.com"), "{md}");
        assert!(md.contains("*File Variable* `host`"), "{md}");
        assert_eq!(
            h.range,
            Some(Range {
                start: pos(2, 6),
                end: pos(2, 10)
            })
        );
    }

    #[test]
    fn percent_prefixed_reference_resolves_as_file_variable() {
        let environment = env(&[]);
        // line 7 `E: {{%host}}`
        let h = hover_at(7, 6, &environment, &NoRequestVariables).unwrap();
        let md = markdown(&h);
        // substitution applies the `%` URL-encoding, so the value is encoded
        assert!(md.contains("https%3A%2F%2Fexample.com"), "{md}");
        assert!(md.contains("*File Variable* `host`"), "{md}");
    }

    #[test]
    fn environment_variable_shows_value() {
        let environment = env(&[("token", "abc")]);
        // line 3 `X-Token: {{token}}` — inner at cols 11..16
        let h = hover_at(3, 12, &environment, &NoRequestVariables).unwrap();
        let md = markdown(&h);
        assert!(md.contains("abc"), "{md}");
        assert!(md.contains("*Environment Variable* `token`"), "{md}");
    }

    #[test]
    fn system_variable_shows_description() {
        let environment = env(&[]);
        // line 4 `V: {{$guid}}`
        let h = hover_at(4, 6, &environment, &NoRequestVariables).unwrap();
        let md = markdown(&h);
        assert!(md.contains("Add a RFC 4122 v4 UUID"), "{md}");
        assert!(md.contains("*System Variable* `$guid`"), "{md}");
    }

    #[test]
    fn request_variable_resolved_text_value() {
        let environment = env(&[]);
        let source = Resolves(RequestVariableResolution::Resolved(ResolvedValue::Text(
            "tok123".to_string(),
        )));
        // line 5 `R: {{login.response.body.$.token}}`
        let h = hover_at(5, 8, &environment, &source).unwrap();
        let md = markdown(&h);
        assert!(md.contains("tok123"), "{md}");
        assert!(md.contains("*Request Variable* `login`"), "{md}");
        // full path range, inside the braces
        let line = "R: {{login.response.body.$.token}}";
        assert_eq!(
            h.range,
            Some(Range {
                start: pos(5, 5),
                end: pos(5, line.len() as u32 - 2)
            })
        );
    }

    #[test]
    fn request_variable_json_value_is_fenced() {
        let environment = env(&[]);
        let source = Resolves(RequestVariableResolution::Resolved(ResolvedValue::Json(
            "{\n  \"a\": 1\n}".to_string(),
        )));
        let h = hover_at(5, 8, &environment, &source).unwrap();
        assert!(markdown(&h).starts_with("```json\n"), "{}", markdown(&h));
    }

    #[test]
    fn request_variable_not_sent_notice() {
        let environment = env(&[]);
        let h = hover_at(5, 8, &environment, &NoRequestVariables).unwrap();
        assert!(
            markdown(&h).contains("has not been sent"),
            "{}",
            markdown(&h)
        );
    }

    #[test]
    fn request_variable_warning_produces_no_hover() {
        let environment = env(&[]);
        let source = Resolves(RequestVariableResolution::Warning(
            "No value is resolved for given JSONPath".to_string(),
        ));
        assert!(hover_at(5, 8, &environment, &source).is_none());
    }

    #[test]
    fn undefined_request_name_falls_through_and_misses() {
        let environment = env(&[]);
        let doc = "GET {{ghost.response.body.*}}";
        let source = Resolves(RequestVariableResolution::Resolved(ResolvedValue::Text(
            "never".to_string(),
        )));
        // `ghost` has no `# @name` definition -> not a request variable, and
        // `ghost.response.body.*` is no file/env variable either -> no hover.
        assert!(hover(doc, pos(0, 8), &environment, Path::new("."), &source).is_none());
    }

    #[test]
    fn undefined_reference_and_plain_text_have_no_hover() {
        let environment = env(&[]);
        // line 6 `N: {{nope}}`
        assert!(hover_at(6, 6, &environment, &NoRequestVariables).is_none());
        // cursor outside any reference
        assert!(hover_at(2, 0, &environment, &NoRequestVariables).is_none());
    }

    #[test]
    fn utf16_positions_with_wide_chars() {
        let environment = env(&[]);
        let doc = "@host = v\n한글: {{host}}";
        // `한글: ` = 4 UTF-16 units -> `host` spans cols 6..10
        let h = hover(doc, pos(1, 7), &environment, Path::new("."), &NoRequestVariables).unwrap();
        assert_eq!(
            h.range,
            Some(Range {
                start: pos(1, 6),
                end: pos(1, 10)
            })
        );
        assert!(markdown(&h).contains("*File Variable* `host`"));
    }

    #[test]
    fn reference_shape_validator_follows_original_regex() {
        assert_eq!(request_variable_reference_name("login."), Some("login"));
        assert_eq!(request_variable_reference_name("login.response"), Some("login"));
        assert_eq!(
            request_variable_reference_name("login.request.headers.X-Req-Id"),
            Some("login")
        );
        assert_eq!(
            request_variable_reference_name("login.response.body.$.a[0]"),
            Some("login")
        );
        assert_eq!(request_variable_reference_name("login.response.body"), Some("login"));
        // no dot after the name -> env/file reference, not a request variable
        assert_eq!(request_variable_reference_name("host"), None);
        // entity must be request|response when a part follows directly
        assert_eq!(request_variable_reference_name("login.body.x"), None);
        // `.headers.` with an empty/invalid header name is not a reference
        assert_eq!(request_variable_reference_name("login.response.headers."), None);
        assert_eq!(
            request_variable_reference_name("login.response.headers.a b"),
            None
        );
        // non-word request name
        assert_eq!(request_variable_reference_name("my-req.response"), None);
    }
}
