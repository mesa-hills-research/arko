; C# highlights for tree-sitter-c-sharp 0.23.5, adapted from nvim-treesitter's
; c_sharp query and the grammar's own (see NOTICE).
;
; When several patterns capture the same node, the highlighter keeps the first,
; so specific patterns come before the `(identifier) @variable` fallback.

; Comments

((comment) @comment.doc
  (#match? @comment.doc "^///"))

((comment) @comment.doc
  (#match? @comment.doc "^/[*][*][^*]"))

(comment) @comment

; Preprocessor

[
  "#define"
  "#elif"
  "#else"
  "#endif"
  "#endregion"
  "#error"
  "#if"
  "#line"
  "#nullable"
  "#pragma"
  "#region"
  "#undef"
  "#warning"
  (shebang_directive)
] @keyword.directive

[
  (preproc_line)
  (preproc_nullable)
  (preproc_pragma)
] @keyword.directive

[
  (preproc_region
    content: (preproc_arg) @comment)
  (preproc_endregion
    content: (preproc_arg) @comment)
]

[
  (preproc_error
    (preproc_arg) @string)
  (preproc_warning
    (preproc_arg) @string)
]

[
  (preproc_define
    (preproc_arg) @constant)
  (preproc_undef
    (preproc_arg) @constant)
  (preproc_pragma
    (identifier) @constant)
]

[
  (preproc_if
    condition: (identifier) @constant)
  (preproc_elif
    condition: (identifier) @constant)
]

; Attributes

(attribute
  name: (identifier) @attribute)

(attribute
  name: (qualified_name
    name: (identifier) @attribute))

(attribute
  name: (generic_name
    (identifier) @attribute))

(attribute_target_specifier
  [
    "event"
    "field"
    "method"
    "param"
    "property"
    "return"
    "type"
    "typevar"
  ] @keyword)

(global_attribute
  [
    "assembly"
    "module"
  ] @keyword)

(attribute_argument
  name: (identifier) @variable.parameter)

; Namespaces

(qualified_name
  qualifier: (identifier) @namespace)

(qualified_name
  qualifier: (qualified_name
    name: (identifier) @namespace))

[
  (namespace_declaration
    name: [
      (identifier) @namespace
      (qualified_name
        name: (identifier) @namespace)
    ])
  (file_scoped_namespace_declaration
    name: [
      (identifier) @namespace
      (qualified_name
        name: (identifier) @namespace)
    ])
]

(using_directive
  name: (identifier) @type)

(using_directive
  name: (identifier)
  (qualified_name
    name: (identifier) @type))

(using_directive
  "static"
  (qualified_name
    name: (identifier) @type))

(using_directive
  [
    (identifier) @namespace
    (qualified_name
      name: (identifier) @namespace)
  ])

(extern_alias_directive
  name: (identifier) @namespace)

((alias_qualified_name
  alias: (identifier) @keyword)
  (#eq? @keyword "global"))

(alias_qualified_name
  alias: (identifier) @namespace)

(qualified_name
  qualifier: (alias_qualified_name
    name: (identifier) @namespace))

; Functions and methods

(method_declaration
  name: (identifier) @function)

(local_function_statement
  name: (identifier) @function)

(constructor_declaration
  name: (identifier) @constructor)

(destructor_declaration
  name: (identifier) @constructor)

(invocation_expression
  function: [
    (identifier) @function
    (generic_name
      .
      (identifier) @function)
    (member_access_expression
      name: [
        (identifier) @function
        (generic_name
          .
          (identifier) @function)
      ])
    (conditional_access_expression
      (member_binding_expression
        name: [
          (identifier) @function
          (generic_name
            .
            (identifier) @function)
        ]))
  ])

; Types

(predefined_type) @type.builtin

(implicit_type) @keyword

[
  (class_declaration
    name: (identifier) @type)
  (struct_declaration
    name: (identifier) @type)
  (record_declaration
    name: (identifier) @type)
  (interface_declaration
    name: (identifier) @type)
  (enum_declaration
    name: (identifier) @type)
  (delegate_declaration
    name: (identifier) @type)
]

(type_parameter
  name: (identifier) @type)

(type_parameter_constraints_clause
  .
  (identifier) @type)

(generic_name
  .
  (identifier) @type)

[
  (type_argument_list
    [
      (identifier) @type
      (qualified_name
        name: (identifier) @type)
    ])
  (base_list
    [
      (identifier) @type
      (qualified_name
        name: (identifier) @type)
    ])
]

(explicit_interface_specifier
  (identifier) @type)

(_
  type: [
    (identifier) @type
    (qualified_name
      name: (identifier) @type)
  ])

(_
  returns: [
    (identifier) @type
    (qualified_name
      name: (identifier) @type)
  ])

(as_expression
  right: (identifier) @type)

; `x is Name` parses as a constant pattern, and Name is a type far more often
; than a constant.
(is_pattern_expression
  pattern: (constant_pattern
    (identifier) @type))

; Properties, fields and members

(property_declaration
  name: (identifier) @property)

(field_declaration
  (modifier
    "const")
  (variable_declaration
    (variable_declarator
      name: (identifier) @constant)))

(field_declaration
  (variable_declaration
    (variable_declarator
      name: (identifier) @property)))

(event_field_declaration
  (variable_declaration
    (variable_declarator
      name: (identifier) @property)))

(event_declaration
  name: (identifier) @property)

(enum_member_declaration
  name: (identifier) @constant)

(member_access_expression
  name: (identifier) @property)

(member_binding_expression
  name: (identifier) @property)

(initializer_expression
  (assignment_expression
    left: (identifier) @property))

(with_initializer
  .
  (identifier) @property)

(tuple_element
  name: (identifier) @property)

(subpattern
  (identifier) @property)

; Parameters

[
  (parameter
    name: (identifier) @variable.parameter)
  (bracketed_parameter_list
    name: (identifier) @variable.parameter)
  (parameter_list
    name: (identifier) @variable.parameter)
]

(implicit_parameter) @variable.parameter

(argument
  name: (identifier) @variable.parameter)

; Labels

(labeled_statement
  (identifier) @label)

(goto_statement
  (identifier) @label)

; Literals

[
  (string_literal)
  (verbatim_string_literal)
  (raw_string_literal)
  (character_literal)
] @string

(interpolated_string_expression
  [
    "\""
    (interpolation_start)
    (interpolation_quote)
    (string_content)
    (raw_string_start)
    (raw_string_end)
  ] @string)

(interpolation_format_clause) @string

(interpolation_brace) @punctuation.special

(escape_sequence) @string.escape

[
  (integer_literal)
  (real_literal)
] @number

(boolean_literal) @boolean

(null_literal) @constant.builtin

(discard) @variable.special

; Keywords

(constructor_initializer
  [
    "base"
    "this"
  ] @keyword)

(indexer_declaration
  "this" @keyword)

[
  "base"
  "this"
] @variable.special

(modifier) @keyword

[
  "__makeref"
  "__reftype"
  "__refvalue"
  "add"
  "alias"
  "and"
  "as"
  "ascending"
  "async"
  "await"
  "break"
  "by"
  "case"
  "catch"
  "checked"
  "class"
  "continue"
  "default"
  "delegate"
  "descending"
  "do"
  "else"
  "enum"
  "equals"
  "event"
  "explicit"
  "extern"
  "file"
  "finally"
  "fixed"
  "for"
  "foreach"
  "from"
  "get"
  "global"
  "goto"
  "group"
  "if"
  "implicit"
  "in"
  "init"
  "interface"
  "into"
  "is"
  "join"
  "let"
  "lock"
  "managed"
  "namespace"
  "new"
  "not"
  "notnull"
  "on"
  "operator"
  "or"
  "orderby"
  "out"
  "params"
  "record"
  "ref"
  "remove"
  "return"
  "scoped"
  "select"
  "set"
  "sizeof"
  "stackalloc"
  "static"
  "struct"
  "switch"
  "throw"
  "try"
  "typeof"
  "unchecked"
  "unmanaged"
  "unsafe"
  "using"
  "when"
  "where"
  "while"
  "with"
  "yield"
] @keyword

(calling_convention
  [
    "Cdecl"
    "Fastcall"
    "Stdcall"
    "Thiscall"
  ] @attribute)

; Identifiers

((identifier) @constant
  (#match? @constant "^[A-Z][A-Z\\d_]+$"))

(identifier) @variable

; Operators and punctuation

[
  "!"
  "!="
  "%"
  "%="
  "&"
  "&&"
  "&="
  "*"
  "*="
  "+"
  "++"
  "+="
  "-"
  "--"
  "-="
  "->"
  ".."
  "/"
  "/="
  "<"
  "<<"
  "<<="
  "<="
  "="
  "=="
  "=>"
  ">"
  ">="
  ">>"
  ">>="
  ">>>"
  ">>>="
  "?"
  "??"
  "??="
  "^"
  "^="
  "|"
  "|="
  "||"
  "~"
] @operator

(conditional_expression
  ":" @operator)

[
  ","
  "."
  ":"
  "::"
  ";"
] @punctuation.delimiter

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  (type_argument_list
    [
      "<"
      ">"
    ] @punctuation.bracket)
  (type_parameter_list
    [
      "<"
      ">"
    ] @punctuation.bracket)
  (function_pointer_type
    [
      "<"
      ">"
    ] @punctuation.bracket)
]
