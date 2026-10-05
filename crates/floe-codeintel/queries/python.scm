; floe-codeintel tags query for Python.
; Pinned copy of tree-sitter-python 0.25.0 queries/tags.scm (MIT, Max Brunsfeld and contributors).
; Functions nested in a class are reported as methods by the extractor (enclosing-definition rule).
; Editing this file changes the extractor string (src/version.rs) and re-extracts every Python blob.

(module (expression_statement (assignment left: (identifier) @name) @definition.constant))

(class_definition
  name: (identifier) @name) @definition.class

(function_definition
  name: (identifier) @name) @definition.function

(call
  function: [
      (identifier) @name
      (attribute
        attribute: (identifier) @name)
  ]) @reference.call
