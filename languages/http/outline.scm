; One outline item per request block: the `### name` separator (when named)
; and the request line itself.

(request_separator
  value: (_) @name) @item

(request
  method: (method)? @context
  url: (_) @name) @item
