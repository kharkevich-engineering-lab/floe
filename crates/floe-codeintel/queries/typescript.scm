; floe-codeintel tags query for TypeScript/TSX, appended to javascript.scm (TypeScript's
; grammar extends JavaScript's, and upstream's tags configuration lists both files).
; Based on tree-sitter-typescript 0.23.2 queries/tags.scm (MIT, Max Brunsfeld and
; contributors). Changes: type aliases, enums and namespaces, and doc comments above exported
; interfaces, type aliases and enums. Editing this file changes the extractor string
; (src/version.rs).

(
  (comment)* @doc
  .
  (export_statement
    declaration: [
      (interface_declaration
        name: (type_identifier) @name) @definition.interface
      (type_alias_declaration
        name: (type_identifier) @name) @definition.type
      (enum_declaration
        name: (identifier) @name) @definition.enum
    ])
  (#strip! @doc "^[\\s\\*/]+|^[\\s\\*/]$")
)

(function_signature
  name: (identifier) @name) @definition.function

(method_signature
  name: (property_identifier) @name) @definition.method

(abstract_method_signature
  name: (property_identifier) @name) @definition.method

(abstract_class_declaration
  name: (type_identifier) @name) @definition.class

(module
  name: (identifier) @name) @definition.module

(internal_module
  name: (identifier) @name) @definition.module

(interface_declaration
  name: (type_identifier) @name) @definition.interface

(type_alias_declaration
  name: (type_identifier) @name) @definition.type

(enum_declaration
  name: (identifier) @name) @definition.enum

(type_annotation
  (type_identifier) @name) @reference.type

(new_expression
  constructor: (identifier) @name) @reference.class
