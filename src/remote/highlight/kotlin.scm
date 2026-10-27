;; Kotlin highlights for tree-sitter-kotlin-ng, using the highlight names of
;; this project's registry.
;;
;; IMPORTANT: Pattern order matters. This project's highlighter keeps the
;; FIRST capture name when multiple patterns match the same range. Therefore
;; specific patterns (function, property, constant…) must appear BEFORE the
;; generic `(identifier) @variable` catch-all at the end of this file.
;;
;; The grammar parses `true`, `false`, `null`, `break`, `continue` and the
;; `class` of `Type::class` as identifiers, so they are matched by their text.

;;; Literals

[
	(line_comment)
	(block_comment)
	(shebang)
] @comment

[
	(number_literal)
	(float_literal)
] @number

((identifier) @boolean
	(#any-of? @boolean "true" "false" "null"))

(character_literal) @string

[
	(string_literal)
	(multiline_string_literal)
] @string

(escape_sequence) @string.escape

; Regex: "pattern".toRegex()
(call_expression
	(navigation_expression
		(string_literal) @string.regex
		(identifier) @_function .
		(#eq? @_function "toRegex")))

; Regex: Regex("pattern")
(call_expression
	. (identifier) @_function
	(#eq? @_function "Regex")
	(value_arguments
		(value_argument
			(string_literal) @string.regex)))

; Regex: Regex.fromLiteral("pattern")
(call_expression
	(navigation_expression
		. (identifier) @_class
		(identifier) @_function .
		(#eq? @_class "Regex")
		(#eq? @_function "fromLiteral"))
	(value_arguments
		(value_argument
			(string_literal) @string.regex)))

;;; String templates (before the brackets, since `}` closes both)

(interpolation
	[
		"$"
		"${"
		"}"
	] @punctuation.special)

; The grammar reads `$name` in a string as string content.
((string_content) @punctuation.special
	(#eq? @punctuation.special "$"))

;;; Keywords

(type_alias "typealias" @keyword)
[
	(class_modifier)
	(member_modifier)
	(function_modifier)
	(property_modifier)
	(platform_modifier)
	(variance_modifier)
	(parameter_modifier)
	(visibility_modifier)
	(reification_modifier)
	(inheritance_modifier)
] @keyword

[
	"val"
	"var"
	"enum"
	"class"
	"object"
	"interface"
	"companion"
	"where"
	"by"
] @keyword

"fun" @keyword

[
	"return"
	"return@"
	"throw"
] @keyword

((identifier) @keyword
	(#any-of? @keyword "break" "continue" "class"))

[
	"if"
	"else"
	"when"
] @keyword

[
	"for"
	"do"
	"while"
] @keyword

[
	"try"
	"catch"
	"finally"
] @keyword

;;; Annotations

(annotation
	"@" @attribute)
(annotation
	(use_site_target) @attribute)
(annotation
	(user_type
		(identifier) @attribute))
(annotation
	(constructor_invocation
		(user_type
			(identifier) @attribute)))

(file_annotation
	"@" @attribute "file" @attribute ":" @attribute)
(file_annotation
	(user_type
		(identifier) @attribute))
(file_annotation
	(constructor_invocation
		(user_type
			(identifier) @attribute)))

;;; Operators & Punctuation

[
	"!"
	"!="
	"!=="
	"="
	"=="
	"==="
	">"
	">="
	"<"
	"<="
	"||"
	"&&"
	"+"
	"++"
	"+="
	"-"
	"--"
	"-="
	"*"
	"*="
	"/"
	"/="
	"%"
	"%="
	"?."
	"?:"
	"!!"
	"is"
	"!is"
	"in"
	"!in"
	"as"
	"as?"
	".."
	"..<"
	"->"
] @operator

[
	"(" ")"
	"[" "]"
	"{" "}"
] @punctuation.bracket

[
	"."
	","
	";"
	":"
	"::"
] @punctuation.delimiter

;;; Package & Imports

(package_header
	"package" @keyword)
(package_header
	(qualified_identifier
		(identifier) @type))

(import
	"import" @keyword)

;;; Labels

(label) @label

;;; Function definitions

(function_declaration
	name: (identifier) @function)

(getter
	"get" @function)
(setter
	"set" @function)

(primary_constructor) @constructor
(secondary_constructor
	"constructor" @constructor)

(constructor_invocation
	(user_type
		(identifier) @constructor))

(anonymous_initializer
	"init" @constructor)

;;; Types

(class_declaration
	name: (identifier) @type)
(object_declaration
	name: (identifier) @type)
(companion_object
	name: (identifier) @type)
(type_alias
	type: (identifier) @type)
(type_parameter
	(identifier) @type)
(user_type
	(identifier) @type)

;;; Function calls — must appear before the generic @variable catch-all

; function()
(call_expression
	. (identifier) @function)

; object.function() or object.property.function()
(call_expression
	(navigation_expression
		(identifier) @function .))

;;; Identifiers — specific patterns before the catch-all

(enum_entry
	(identifier) @constant)

; The name, which the type follows, rather than an identifier as the default value.
(class_parameter
	(identifier) @property . (_))

(class_body
	(property_declaration
		(variable_declaration
			(identifier) @property)))

(navigation_expression
	(identifier) @property .)

(parameter
	(identifier) @variable)

(lambda_literal
	(lambda_parameters
		(variable_declaration
			(identifier) @variable)))

; `this` / `super` keywords
(this_expression) @variable.special
(super_expression) @variable.special

; `it` keyword inside lambdas
((identifier) @variable.special
(#eq? @variable.special "it"))

; `field` keyword inside property getter/setter
((identifier) @variable.special
(#eq? @variable.special "field"))

; Generic identifier catch-all — MUST be last so specific patterns win
(identifier) @variable
