; Adapted from rest-nvim/tree-sitter-http queries/highlights.scm (MIT, NTBBloodbath)
; using Zed highlight capture conventions.

; Request line
(method) @keyword

(request
  url: (_) @link_uri)

(http_version) @constant

; Headers
(header
  name: (_) @property)

(header
  ":" @punctuation.delimiter)

; Variables
(variable_declaration
  name: (identifier) @variable)

(variable
  name: (_) @variable)

[
  "{{"
  "}}"
] @punctuation.bracket

; Operators
(variable_declaration
  "=" @operator)

(comment
  "=" @operator)

; Metadata comments (# @name foo, # @no-redirect, ...)
(comment
  "@" @keyword
  name: (_) @keyword)

; Response
(status_code) @number

(status_text) @string

; External body (< ./payload.json)
(external_body
  path: (_) @string.special)

; Comments and request separators (### ...)
[
  (comment)
  (request_separator)
] @comment
