# 形式语法

本章给出 Gugu 的规范语法。词法记号、字符串转义、换行续行和最长匹配见[词法结构](lexical.md)；本章的产生式在词法分析之后使用。产生式中的 `X?`、`X*`、`X+` 分别表示可选、重复零次以上和重复一次以上，`[...]` 表示可选项，`|` 表示选择，`ε` 表示空；这些元符号不属于 Gugu 源码。

除特别注明外，产生式之间的空白可以是空格、制表符或允许的换行。换行是否产生语句终止由[词法结构](lexical.md)的“未完成则续行”规则决定。语法错误、无法消歧的名称解析和不满足静态约束的程序都是编译错误。

## 词法记号

```text
IDENT       = ASCII 字母或 '_' 开头，后接 ASCII 字母、数字或 '_' 的序列；
              但不能是关键字
INT         = 十进制、十六进制、二进制或八进制整数记号；
FLOAT       = 含小数点或指数的浮点记号；
CHAR        = 字符记号；
BYTE_CHAR   = 字节字符记号；
STRING      = 普通、插值、raw、字节或 C 字符串记号；
ATTRIBUTE   = 已通过词法分析的属性内容；
```

`-` 不属于 `INT` 或 `FLOAT` 的一部分；负数由一元 `-` 表达。`NEWLINE` 只在上一记号不能继续当前语法结构时终止语句。注释在语法分析前删除，但文档注释作为附着信息保留。

## 文件、属性与声明

```ebnf
source_file         ::= module_attribute* source_item* EOF ;
module_attribute    ::= "#![" attribute "]" ;
source_item         ::= item | source_macro_item ;
item                ::= attribute* visibility? declaration ;
source_macro_item   ::= attribute* "comptime" "source" block ;
attribute           ::= "#[" ATTRIBUTE "]" ;
visibility          ::= "pub" ;

declaration         ::= use_declaration
                      | function_declaration
                      | struct_declaration
                      | enum_declaration
                      | union_declaration
                      | trait_declaration
                      | impl_declaration
                      | const_declaration
                      | type_declaration
                      | static_declaration
                      | extern_declaration
                      | global_asm_declaration ;

use_declaration     ::= "use" use_tree terminator ;
use_tree            ::= path ["as" IDENT]
                      | path ".{" use_item ("," use_item)* [","] "}" ;
use_item            ::= IDENT ["as" IDENT] ;

function_declaration ::= "unsafe"? "fn" IDENT generic_parameters?
                         "(" parameter_list? ")" return_type? function_body ;
function_body       ::= block | "=" expression terminator ;

struct_declaration  ::= "struct" IDENT generic_parameters? struct_body ;
struct_body         ::= "(" newtype_field ")" terminator
                      | "{" field_list? "}" ;
newtype_field       ::= visibility? type ;
field_list          ::= field (field_separator field)* [field_separator] ;
field               ::= visibility? IDENT ":" type ;

enum_declaration    ::= "enum" IDENT generic_parameters? "{" variant_list? "}" ;
variant_list        ::= variant (variant_separator variant)* [variant_separator] ;
variant              ::= IDENT
                      | IDENT "(" type_list? ")"
                      | IDENT "{" field_list? "}" ;

union_declaration   ::= "union" IDENT generic_parameters? "{" field_list? "}" ;

trait_declaration   ::= "unsafe"? "trait" IDENT generic_parameters? "{" trait_item* "}" ;
trait_item          ::= associated_type
                      | associated_const
                      | function_signature
                      | function_declaration ;
associated_type     ::= "type" IDENT ["=" type] terminator ;
associated_const    ::= "const" IDENT [":" type] ["=" expression] terminator ;
function_signature  ::= "fn" IDENT generic_parameters? "(" parameter_list? ")"
                         return_type? terminator ;
impl_declaration    ::= "unsafe"? "impl" generic_parameters? impl_target "{" trait_item* "}" ;
impl_target         ::= type ["for" type] ;

const_declaration   ::= "const" IDENT [":" type] "=" expression terminator ;
type_declaration    ::= "type" IDENT generic_parameters? "=" (type | impl_type) terminator ;
static_declaration  ::= "static" IDENT ":" type "=" expression terminator ;
extern_declaration  ::= "extern" "\"C\"" (extern_function | "{" extern_item* "}") ;
extern_function     ::= "unsafe"? "fn" IDENT generic_parameters?
                        "(" parameter_list? ")" return_type? (function_body | terminator) ;
extern_item         ::= attribute* "fn" IDENT "(" parameter_list? ")" return_type? terminator ;
global_asm_declaration ::= "global_asm" "(" STRING ")" terminator ;
```

模块顶层的可见性、`extern`、属性适用范围、`type = impl Trait` 的不透明身份以及各声明的静态约束见[声明与模块](declarations.md)、[类型系统](types.md)和[unsafe 与 intrinsic](unsafe.md)。`field_separator`、`variant_separator`和`terminator`可以是逗号或规范允许的换行；字段和变体的尾逗号合法，顶层声明不能靠逗号分隔。

## 泛型、参数与类型

```ebnf
generic_parameters ::= "[" generic_parameter ("," generic_parameter)* [","] "]" ;
generic_parameter  ::= IDENT [":" bounds] ["..."]
                      | "comptime" IDENT ":" type ;
bounds             ::= bound ("+" bound)* ;
bound              ::= path generic_arguments?
                      | "Fn" "(" type_list? ")" type ;
parameter_list     ::= parameter ("," parameter)* [","] ;
parameter          ::= "comptime" pattern ":" type
                      | pattern [":" type]
                      | "..." IDENT ":" reference_type ;
return_type        ::= type ;
type_list          ::= type ("," type)* [","] ;

type               ::= "!"
                      | "Self"
                      | path generic_arguments?
                      | reference_type
                      | raw_pointer_type
                      | function_type
                      | array_type
                      | tuple_type
                      | "dyn" dyn_bounds
                      | impl_type
                      | source_macro_type ;
source_macro_type   ::= "comptime" "source" block ;
reference_type     ::= "&" type ;
raw_pointer_type   ::= "*" type ;
function_type      ::= "fn" "(" type_list? ")" [type] ;
tuple_type         ::= "(" ")"
                      | "(" type "," type_list? ")" ;
generic_arguments  ::= "[" generic_argument ("," generic_argument)* [","] "]" ;
generic_argument   ::= type | expression ;
dyn_bounds         ::= path ("+" path)* ;
impl_type          ::= "impl" bound ("+" bound)* ;
path               ::= IDENT ("." IDENT)* ("::" IDENT)* ;
```

函数、方法的显式类型实参使用 `path :: generic_arguments`；类型名后的方括号是类型实参；值后的方括号是下标。关键字构造器 `chan[T](n)`、`size_of[T]()`、`align_of[T]()`、`offset_of[T](field)`和`type_id[T]()`按[类型系统](types.md)的专门规则解析。

`comptime source` 在模块项列表、块语句列表、表达式、类型和模式位置使用相同的表面记号，由所在语法位置决定其 source slot。`source` 在这里是跟随 `comptime` 的上下文词，不是保留关键字；解析器保留该节点，不把脚本块本身当成生成结果。宏脚本返回的 `ParsedSource` 必须与该 source slot 的片段类别相容；具体展开规则见[编译期执行](comptime.md)。

## 块、语句与控制流

```ebnf
block               ::= "{" statement* [expression] "}" ;
statement           ::= let_statement
                      | assignment_statement
                      | defer_statement
                      | source_macro_statement
                      | "yield" terminator
                      | expression terminator ;
source_macro_statement ::= "comptime" "source" block terminator ;

let_statement       ::= "let" pattern [":" type] ["=" expression]
                        ["else" block] terminator ;
assignment_statement ::= place assignment_operator expression terminator ;
assignment_operator ::= "=" | "+=" | "-=" | "*=" | "/=" | "%="
                      | "&=" | "|=" | "^=" | "<<=" | ">>=" ;
defer_statement     ::= "defer" ["ret"] (expression | block) terminator ;
terminator          ::= ";" | NEWLINE ;
field_separator     ::= "," | NEWLINE ;
variant_separator   ::= "," | NEWLINE ;
place               ::= postfix_expression ;
```

`let`带初始化器时的模式必须可判定；先声明后赋值只允许简单标识符。`expression terminator`中的分号仅表示丢弃该表达式的值；没有分号的最后表达式是块值。块为空时值为 `()`。赋值不是表达式，因此不能嵌套在调用参数、运算符或条件中。

```ebnf
expression          ::= attribute* expression_core ;
expression_core     ::= if_expression
                      | match_expression
                      | try_expression
                      | loop_expression
                      | while_expression
                      | for_expression
                      | select_expression
                      | async_expression
                      | closure_expression
                      | return_expression
                      | break_expression
                      | continue_expression
                      | logical_or_expression ;
```

`attribute*` 是表达式的语法前缀；属性的可附着位置仍由[词法 · 属性](lexical.md#属性适用位置与冲突)限制。例如 `#[ffi(bridge)]` 可以放在调用表达式前，但不是运行时包装函数。

```ebnf
if_expression       ::= "if" condition block ["else" (if_expression | block)] ;
condition           ::= condition_part ("&&" condition_part)* ;
condition_part      ::= "let" pattern "=" expression
                      | comparison_expression ("||" comparison_expression)* ;

match_expression    ::= "match" expression "{" match_arm* "}" ;
match_arm           ::= pattern ["if" expression] "=>" expression [("," | NEWLINE)] ;
try_expression      ::= "try" block ;
loop_expression     ::= "loop" block ;
while_expression    ::= "while" condition block ;
for_expression      ::= "for" pattern "in" expression block ;

async_expression    ::= "async" (postfix_expression | block) ;
closure_expression  ::= "fn" "(" parameter_list? ")" return_type? function_body ;
return_expression   ::= "return" [expression] ;
break_expression    ::= "break" [expression] ;
continue_expression ::= "continue" ;
```

`comptime source` 在表达式位置通过 `primary_expression` 解析，不作为普通值表达式的隐式常量折叠。

```ebnf
select_expression   ::= "select" "{" select_arm* "}" ;
select_arm          ::= send_operation "=>" expression [("," | NEWLINE)]
                      | "let" pattern "=" receive_operation "=>" expression [("," | NEWLINE)]
                      | "let" pattern "=" wait_operation "=>" expression [("," | NEWLINE)]
                      | "_" "=>" expression [("," | NEWLINE)] ;
send_operation      ::= postfix_expression "." "send" "(" expression ")" ;
receive_operation   ::= postfix_expression "." "recv" "(" ")" ;
wait_operation      ::= postfix_expression "." "wait" "(" ")" ;
```

`async` 的非块操作数在静态检查时必须是一次完整调用；其它 postfix 表达式是编译错误。`select` 分支的操作数只能是相应的语言操作；`try_send` 和 `try_recv` 不能出现在分支中。接收和等待分支模式必须不可驳，绑定只在该分支体内有效。没有分支的 `select {}` 合法，并永久挂起当前协程。

## 运算符与后缀

```ebnf
logical_or_expression  ::= logical_and_expression ("||" logical_and_expression)* ;
logical_and_expression ::= comparison_expression ("&&" comparison_expression)* ;
comparison_expression  ::= range_expression [("==" | "!=" | "<" | "<=" | ">" | ">=") range_expression] ;
range_expression       ::= bit_or_expression [".." bit_or_expression] ;
bit_or_expression      ::= bit_xor_expression ("|" bit_xor_expression)* ;
bit_xor_expression     ::= bit_and_expression ("^" bit_and_expression)* ;
bit_and_expression     ::= shift_expression ("&" shift_expression)* ;
shift_expression       ::= additive_expression (("<<" | ">>") additive_expression)* ;
additive_expression    ::= multiplicative_expression (("+" | "-") multiplicative_expression)* ;
multiplicative_expression ::= unary_expression (("*" | "/" | "%") unary_expression)* ;
unary_expression       ::= ("!" | "-" | "~" | "&" | "*") unary_expression
                          | postfix_expression ;

postfix_expression     ::= primary_expression postfix* ;
postfix                ::= call_suffix
                          | "::" generic_arguments
                          | index_suffix
                          | field_suffix
                          | "?"
                          | "." "match" "{" match_arm* "}" ;
call_suffix             ::= "(" argument_list? ")" ;
index_suffix            ::= "[" (expression | range_index) "]" ;
range_index             ::= [expression] ".." [expression] ;
field_suffix            ::= "." (IDENT | INT) ;
argument_list           ::= expression ("," expression)* [","] ;

primary_expression      ::= IDENT
                          | literal
                          | path
                          | block
                          | "unsafe" block
                          | source_macro_expression
                          | "comptime" (block | unary_expression)
                          | intrinsic_expression
                          | asm_expression
                          | "(" expression ")"
                          | "(" tuple_elements? ")"
                          | "[" array_elements? "]"
                          | "[" expression ";" expression "]"
                          | struct_expression
                          | enum_expression
                          | "chan" generic_arguments "(" expression ")" ;
source_macro_expression ::= "comptime" "source" block ;
intrinsic_expression    ::= ("size_of" | "align_of" | "type_id") generic_arguments "(" ")"
                          | "offset_of" generic_arguments "(" (IDENT | INT) ")"
                          | "type_id_count" "(" ")" ;
asm_expression          ::= "asm" "(" STRING ("," asm_operand)* [","] ")" ;
asm_operand             ::= "in" "(" STRING ")" expression
                          | "out" "(" STRING ")" place
                          | "lateout" "(" STRING ")" place
                          | "clobber" "(" STRING ("," STRING)* ")" ;
```

算术、移位和位运算符左结合；比较和 `..` 不结合，不能连续出现；`&&`、`||` 从左到右短路。赋值类运算符在语句层右侧求值。`async` 和一元运算符属于前缀层；后缀 `.match { ... }` 与调用、字段、索引和 `?` 同层。具体优先级及 `&`、`!` 在类型位置与表达式位置的区分见[表达式与语句](expressions.md)。

## 值构造与模式

```ebnf
literal             ::= INT | FLOAT | CHAR | BYTE_CHAR | STRING
                      | "true" | "false" ;
array_elements      ::= expression ("," expression)* [","] ;
tuple_elements      ::= expression "," [expression ("," expression)* [","]] ;
struct_expression   ::= path "{" named_value_list? "}" ;
named_value_list    ::= named_value (field_separator named_value)* [field_separator] ;
named_value         ::= IDENT ":" expression | IDENT ;
enum_expression     ::= path ("(" argument_list? ")" | "{" named_value_list? "}" | ε) ;

pattern             ::= or_pattern ;
or_pattern          ::= at_pattern ("|" at_pattern)* ;
at_pattern          ::= [IDENT "@"] pattern_atom ;
pattern_atom        ::= "_" | IDENT | literal
                      | range_pattern | tuple_pattern | array_pattern
                      | struct_pattern | constructor_pattern | "&" pattern
                      | source_macro_pattern ;
source_macro_pattern ::= "comptime" "source" block ;
range_pattern       ::= expression ".." expression ;
tuple_pattern       ::= "(" ")" | "(" pattern "," pattern_list? ")" ;
array_pattern       ::= "[" pattern_list? ["," rest_pattern] "]" ;
rest_pattern        ::= ".." | IDENT "@" ".." ;
struct_pattern      ::= path "{" field_pattern_list? ["," ".."] "}" ;
field_pattern_list  ::= field_pattern ("," field_pattern)* ;
field_pattern       ::= IDENT [":" pattern] ;
constructor_pattern ::= path ("(" pattern_list? ")" | "{" field_pattern_list? "}")" ;
pattern_list        ::= pattern ("," pattern)* [","] ;
```

模式中的 `range_pattern` 两端必须是同一类型的编译期整数或字符常量；值表达式的 `a..b` 只能产生整数 `Range`。`constructor_pattern`的路径必须解析为当前待匹配枚举或 newtype 的构造器。模式绑定、穷尽性、守卫和 rest 的静态限制见[模式](patterns.md)。

## 解析后的静态约束

1. 名称解析、可见性检查和类型检查在生成机器码前完成。源文件中被 `cfg` 裁掉的项不进入这些阶段。
2. 语法合法不表示程序合法：不可驳约束、穷尽匹配、未初始化读取、泛型约束、`unsafe` 前置条件和 FFI ABI 约束仍分别检查。
3. 产生式中的 `path` 只是语法形式；它最终只能解析为一个模块项、类型、构造器、方法或关联项。多个候选且无法由接收者、期望类型或显式实参消歧时是编译错误。
4. `comptime source` 的脚本必须返回 `ParsedSource` 或 `Result[ParsedSource, E]`；解析成功后生成的片段重新进入适用的 `cfg`、定义收集、名称解析、类型检查和 HIR 阶段。生成的源码可以包含新的 `comptime source`，但必须服从展开预算。
5. 语法树必须保留属性、文档注释、每个表达式的源范围以及插值片段的源范围，供诊断、`track_caller`、宏展开链和文档测试使用。
