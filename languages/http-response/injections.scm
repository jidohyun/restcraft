; Dynamic body injection driven by the Content-Type header.
;
; The grammar splits the mime subtype on the RFC 6839 structured-syntax
; suffix, so (content_subtype) text is already a language name for the
; common cases:
;   application/json            -> "json"
;   text/html                   -> "html"
;   application/xml, image/svg+xml -> "xml"
;   text/css                    -> "css"
;   text/javascript             -> "javascript"
;
; Zed resolves @injection.language capture text against language names and
; path suffixes case-insensitively (same mechanism as markdown fenced code
; blocks). Subtypes with no matching language ("plain", "png",
; "x-www-form-urlencoded", ...) simply produce no injection.
(document
  (header
    (content_type_value
      (content_subtype) @injection.language))
  (body) @injection.content)
