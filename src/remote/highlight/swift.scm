; Swift highlights for tree-sitter-swift 0.7.4, adapted from the grammar's own
; query (see NOTICE).
;
; When several patterns capture the same node, the highlighter keeps the first,
; so specific patterns come before the `(simple_identifier) @variable` fallback.

; Comments

((comment) @comment.doc
  (#match? @comment.doc "^///"))

((multiline_comment) @comment.doc
  (#match? @comment.doc "^/[*][*][^*]"))

[
  (comment)
  (multiline_comment)
] @comment

; Compiler directives and macros

[
  (directive)
  (diagnostic)
  (shebang_line)
] @keyword.directive

(directive
  (simple_identifier) @constant)

(macro_invocation
  "#" @function
  (simple_identifier) @function)

[
  (availability_condition)
  (playground_literal)
  (key_path_string_expression)
  (selector_expression)
  (external_macro_definition)
] @function.macro

(special_literal) @constant.builtin

; Attributes and property wrappers

(attribute
  "@" @attribute
  (user_type
    (type_identifier) @attribute))

[
  "@autoclosure"
  "@escaping"
] @attribute

((parameter_modifier) @keyword
  (#not-match? @keyword "^@"))

; Declarations

(import_declaration
  (identifier
    (simple_identifier) @namespace))

(function_declaration
  name: (simple_identifier) @function)

(protocol_function_declaration
  name: (simple_identifier) @function)

(macro_declaration
  (simple_identifier) @function)

(init_declaration
  "init" @constructor)

(enum_entry
  name: (simple_identifier) @constant)

[
  (class_body
    (property_declaration
      name: (pattern
        bound_identifier: (simple_identifier) @property)))
  (enum_class_body
    (property_declaration
      name: (pattern
        bound_identifier: (simple_identifier) @property)))
  (protocol_property_declaration
    name: (pattern
      bound_identifier: (simple_identifier) @property))
]

(parameter
  external_name: (simple_identifier) @variable.parameter)

(parameter
  name: (simple_identifier) @variable.parameter)

(lambda_parameter
  name: (simple_identifier) @variable.parameter)

(lambda_literal
  "in" @keyword)

(statement_label) @label

(operator_declaration
  (simple_identifier) @type)

(precedence_group_declaration
  (simple_identifier) @type)

(precedence_group_attribute
  .
  (simple_identifier) @property)

((precedence_group_attribute
  (simple_identifier) @constant .)
  (#any-of? @constant "left" "right" "none"))

(precedence_group_attribute
  (simple_identifier) @type .)

; Types

[
  (inheritance_constraint
    constrained_type: (identifier
      (simple_identifier) @type))
  (equality_constraint
    constrained_type: (identifier
      (simple_identifier) @type))
]

(type_identifier) @type

; Calls and members

; The grammar parses `defer { ... }` as a call with a trailing closure.
((call_expression
  (simple_identifier) @keyword)
  (#eq? @keyword "defer"))

((call_expression
  (simple_identifier) @type)
  (#match? @type "^[A-Z]"))

(call_expression
  (simple_identifier) @function)

(call_expression
  (navigation_expression
    suffix: (navigation_suffix
      suffix: (simple_identifier) @function)))

(call_expression
  (prefix_expression
    "."
    (simple_identifier) @function))

; A subscript such as `items[index]` parses as a call. These patterns match at the
; closing bracket, after the call patterns above, so their captures take precedence.
(call_expression
  (simple_identifier) @variable
  (call_suffix
    (value_arguments
      "["
      "]")))

(call_expression
  (navigation_expression
    suffix: (navigation_suffix
      suffix: (simple_identifier) @property))
  (call_suffix
    (value_arguments
      "["
      "]")))

((navigation_expression
  target: (simple_identifier) @type)
  (#match? @type "^[A-Z]"))

(navigation_suffix
  suffix: (simple_identifier) @property)

[
  (prefix_expression
    "."
    (simple_identifier) @constant)
  (pattern
    "."
    (simple_identifier) @constant)
]

(value_argument
  name: (value_argument_label
    (simple_identifier) @variable.parameter))

; Identifiers

[
  (self_expression)
  (super_expression)
] @variable.special

((simple_identifier) @variable.special
  (#match? @variable.special "^(\\$.*|_)$"))

(wildcard_pattern) @variable.special

(simple_identifier) @variable

; Literals

[
  "\""
  "\"\"\""
  (line_str_text)
  (multi_line_str_text)
  (raw_str_part)
  (raw_str_end_part)
] @string

(str_escaped_char) @string.escape

[
  (line_string_literal
    [
      "\\("
      ")"
    ] @punctuation.special)
  (multi_line_string_literal
    [
      "\\("
      ")"
    ] @punctuation.special)
  (raw_str_interpolation
    [
      (raw_str_interpolation_start)
      ")"
    ] @punctuation.special)
]

(regex_literal) @string.regex

[
  (integer_literal)
  (hex_literal)
  (oct_literal)
  (bin_literal)
  (real_literal)
] @number

(boolean_literal) @boolean

(nil_literal) @constant.builtin

; Keywords

[
  (visibility_modifier)
  (member_modifier)
  (function_modifier)
  (property_modifier)
  (inheritance_modifier)
  (mutation_modifier)
  (ownership_modifier)
  (property_behavior_modifier)
  (throws)
  (where_keyword)
  (else)
  (as_operator)
  (try_operator)
  (throw_keyword)
  (catch_keyword)
  (default_keyword)
  (getter_specifier)
  (setter_specifier)
  (modify_specifier)
] @keyword

[
  "actor"
  "any"
  "as"
  "associatedtype"
  "async"
  "await"
  "borrowing"
  "break"
  "case"
  "class"
  "consume"
  "consuming"
  "continue"
  "convenience"
  "deinit"
  "didSet"
  "discard"
  "do"
  "each"
  "enum"
  "extension"
  "fallthrough"
  "for"
  "func"
  "guard"
  "if"
  "import"
  "in"
  "indirect"
  "infix"
  "inout"
  "is"
  "let"
  "macro"
  "nonisolated"
  "operator"
  "override"
  "postfix"
  "precedencegroup"
  "prefix"
  "protocol"
  "repeat"
  "required"
  "return"
  "some"
  "struct"
  "subscript"
  "switch"
  "typealias"
  "unowned"
  "unsafe"
  "var"
  "weak"
  "while"
  "willSet"
  "yield"
] @keyword

; Operators and punctuation

[
  "!="
  "!=="
  "%"
  "%="
  "&"
  "&&"
  "*"
  "*="
  "+"
  "++"
  "+="
  "-"
  "--"
  "-="
  "->"
  "..."
  "..<"
  "/"
  "/="
  "<"
  "<<"
  "<="
  "="
  "=="
  "==="
  ">"
  ">="
  ">>"
  "?"
  "??"
  "\\"
  "^"
  "|"
  "||"
  "~"
  (bang)
  (custom_operator)
] @operator

(ternary_expression
  ":" @operator)

[
  "."
  ";"
  ":"
  ","
] @punctuation.delimiter

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

(type_arguments
  [
    "<"
    ">"
  ] @punctuation.bracket)

(type_parameters
  [
    "<"
    ">"
  ] @punctuation.bracket)
