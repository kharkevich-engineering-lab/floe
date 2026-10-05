; floe-codeintel tags query for Java.
; Based on tree-sitter-java 0.23.5 queries/tags.scm (MIT, Ayman Nadeem, Max Brunsfeld and
; contributors). Changes: enums and constructors. Editing this file changes the extractor
; string (src/version.rs).

(class_declaration
  name: (identifier) @name) @definition.class

(enum_declaration
  name: (identifier) @name) @definition.enum

(method_declaration
  name: (identifier) @name) @definition.method

(constructor_declaration
  name: (identifier) @name) @definition.method

(method_invocation
  name: (identifier) @name
  arguments: (argument_list) @reference.call)

(interface_declaration
  name: (identifier) @name) @definition.interface

(type_list
  (type_identifier) @name) @reference.implementation

(object_creation_expression
  type: (type_identifier) @name) @reference.class

(superclass (type_identifier) @name) @reference.class
