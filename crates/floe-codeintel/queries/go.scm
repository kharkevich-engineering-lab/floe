; floe-codeintel tags query for Go.
; Based on tree-sitter-go 0.25.0 queries/tags.scm (MIT, Max Brunsfeld and contributors).
; Changes: `#set-adjacent!` (a typo upstream, ignored by every consumer) is `#select-adjacent!`;
; method receivers are captured as the container; struct/interface/const definitions get
; their kinds (upstream has those patterns without a definition capture); package, import and
; var patterns without captures are dropped. Editing this file changes the extractor string.

(
  (comment)* @doc
  .
  (function_declaration
    name: (identifier) @name) @definition.function
  (#strip! @doc "^//\\s*")
  (#select-adjacent! @doc @definition.function)
)

(
  (comment)* @doc
  .
  (method_declaration
    receiver: (parameter_list
      (parameter_declaration
        type: [
          (type_identifier) @receiver
          (pointer_type (type_identifier) @receiver)
          (generic_type type: (type_identifier) @receiver)
          (pointer_type (generic_type type: (type_identifier) @receiver))
        ]))
    name: (field_identifier) @name) @definition.method
  (#strip! @doc "^//\\s*")
  (#select-adjacent! @doc @definition.method)
)

(
  (comment)* @doc
  .
  (method_declaration
    name: (field_identifier) @name) @definition.method
  (#strip! @doc "^//\\s*")
  (#select-adjacent! @doc @definition.method)
)

(call_expression
  function: [
    (identifier) @name
    (parenthesized_expression (identifier) @name)
    (selector_expression field: (field_identifier) @name)
    (parenthesized_expression (selector_expression field: (field_identifier) @name))
  ]) @reference.call

(type_spec
  name: (type_identifier) @name
  type: (struct_type)) @definition.struct

(type_spec
  name: (type_identifier) @name
  type: (interface_type)) @definition.interface

(type_spec
  name: (type_identifier) @name) @definition.type

(type_identifier) @name @reference.type

(const_spec
  name: (identifier) @name) @definition.const
