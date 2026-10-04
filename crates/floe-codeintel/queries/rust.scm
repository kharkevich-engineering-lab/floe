; floe-codeintel tags query for Rust.
; Based on tree-sitter-rust 0.24.2 queries/tags.scm (MIT, Maxim Sokolov, Max Brunsfeld and
; contributors). Changes: precise kinds (struct, enum, trait, type) instead of class/interface,
; const/static items, trait method signatures, and an `impl` scope so methods carry their type
; as container. Editing this file changes the extractor string (src/version.rs).

; ADT definitions

(struct_item
    name: (type_identifier) @name) @definition.struct

(enum_item
    name: (type_identifier) @name) @definition.enum

(union_item
    name: (type_identifier) @name) @definition.struct

; type aliases

(type_item
    name: (type_identifier) @name) @definition.type

; constants and statics

(const_item
    name: (identifier) @name) @definition.const

(static_item
    name: (identifier) @name) @definition.const

; method definitions

(declaration_list
    (function_item
        name: (identifier) @name) @definition.method)

(declaration_list
    (function_signature_item
        name: (identifier) @name) @definition.method)

; function definitions

(function_item
    name: (identifier) @name) @definition.function

; trait definitions
(trait_item
    name: (type_identifier) @name) @definition.trait

; module definitions
(mod_item
    name: (identifier) @name) @definition.module

; macro definitions

(macro_definition
    name: (identifier) @name) @definition.macro

; impl blocks: not definitions, but the container of the methods inside them

(impl_item
    type: (_) @name) @scope.impl

; references

(call_expression
    function: (identifier) @name) @reference.call

(call_expression
    function: (field_expression
        field: (field_identifier) @name)) @reference.call

(call_expression
    function: (scoped_identifier
        name: (identifier) @name)) @reference.call

(macro_invocation
    macro: (identifier) @name) @reference.call

; implementations

(impl_item
    trait: (type_identifier) @name) @reference.implementation

(impl_item
    type: (type_identifier) @name
    !trait) @reference.implementation
