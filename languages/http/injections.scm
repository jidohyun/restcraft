; Adapted from rest-nvim/tree-sitter-http queries/injections.scm (MIT, NTBBloodbath).

((json_body) @injection.content
  (#set! injection.language "json"))

((xml_body) @injection.content
  (#set! injection.language "xml"))

((graphql_data) @injection.content
  (#set! injection.language "graphql"))
