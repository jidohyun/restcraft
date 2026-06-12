; Meta comment lines (`# get-user | 12ms | 344B`).
(comment) @comment

; --- Status line ---------------------------------------------------------
(http_version) @keyword

; One mutually-exclusive pattern per status class so highlighting does not
; depend on pattern-precedence behavior.
((status_code) @number
  (#match? @number "^1"))
((status_code) @string.special
  (#match? @string.special "^2"))
((status_code) @constant
  (#match? @constant "^3"))
((status_code) @keyword
  (#match? @keyword "^4"))
((status_code) @function
  (#match? @function "^5"))

(reason_phrase) @string

; --- Headers ---------------------------------------------------------------
(header (header_name) @property)
(header (header_value) @string)

; Content-Type pieces: the injectable subtype stands out from the rest.
(mime_main) @string
(subtype_prefix) @string
(content_subtype) @string.special
(mime_params) @string
