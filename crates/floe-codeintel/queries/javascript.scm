; floe-codeintel tags query for JavaScript/JSX (also the first half of TypeScript/TSX).
; Based on tree-sitter-javascript 0.25.0 queries/tags.scm (MIT, Max Brunsfeld and
; contributors). Change: the first block, doc comments above an `export` declaration (upstream
; anchors them to the inner declaration, so `/** … */ export function f` lost its doc). It comes
; first so it wins over the generic patterns for the same name. Editing this file changes the
; extractor string (src/version.rs) and re-extracts every JavaScript and TypeScript blob.

(
  (comment)* @doc
  .
  (export_statement
    declaration: [
      (function_declaration
        name: (identifier) @name)
      (generator_function_declaration
        name: (identifier) @name)
    ] @definition.function)
  (#strip! @doc "^[\\s\\*/]+|^[\\s\\*/]$")
  (#select-adjacent! @doc @definition.function)
)

(
  (comment)* @doc
  .
  (export_statement
    declaration: (class_declaration
      name: (_) @name) @definition.class)
  (#strip! @doc "^[\\s\\*/]+|^[\\s\\*/]$")
  (#select-adjacent! @doc @definition.class)
)

(
  (comment)* @doc
  .
  (export_statement
    declaration: (lexical_declaration
      (variable_declarator
        name: (identifier) @name
        value: [(arrow_function) (function_expression)]) @definition.function))
  (#strip! @doc "^[\\s\\*/]+|^[\\s\\*/]$")
  (#select-adjacent! @doc @definition.function)
)

(
  (comment)* @doc
  .
  (method_definition
    name: (property_identifier) @name) @definition.method
  (#not-eq? @name "constructor")
  (#strip! @doc "^[\\s\\*/]+|^[\\s\\*/]$")
  (#select-adjacent! @doc @definition.method)
)

(
  (comment)* @doc
  .
  [
    (class
      name: (_) @name)
    (class_declaration
      name: (_) @name)
  ] @definition.class
  (#strip! @doc "^[\\s\\*/]+|^[\\s\\*/]$")
  (#select-adjacent! @doc @definition.class)
)

(
  (comment)* @doc
  .
  [
    (function_expression
      name: (identifier) @name)
    (function_declaration
      name: (identifier) @name)
    (generator_function
      name: (identifier) @name)
    (generator_function_declaration
      name: (identifier) @name)
  ] @definition.function
  (#strip! @doc "^[\\s\\*/]+|^[\\s\\*/]$")
  (#select-adjacent! @doc @definition.function)
)

(
  (comment)* @doc
  .
  (lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function) (function_expression)]) @definition.function)
  (#strip! @doc "^[\\s\\*/]+|^[\\s\\*/]$")
  (#select-adjacent! @doc @definition.function)
)

(
  (comment)* @doc
  .
  (variable_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function) (function_expression)]) @definition.function)
  (#strip! @doc "^[\\s\\*/]+|^[\\s\\*/]$")
  (#select-adjacent! @doc @definition.function)
)

(assignment_expression
  left: [
    (identifier) @name
    (member_expression
      property: (property_identifier) @name)
  ]
  right: [(arrow_function) (function_expression)]
) @definition.function

(pair
  key: (property_identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.function

(
  (call_expression
    function: (identifier) @name) @reference.call
  (#not-match? @name "^(require)$")
)

(call_expression
  function: (member_expression
    property: (property_identifier) @name)
  arguments: (_) @reference.call)

(new_expression
  constructor: (_) @name) @reference.class

(export_statement value: (assignment_expression left: (identifier) @name right: ([
 (number)
 (string)
 (identifier)
 (undefined)
 (null)
 (new_expression)
 (binary_expression)
 (call_expression)
]))) @definition.constant
