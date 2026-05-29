#!/usr/bin/env ruby
# frozen_string_literal: true
#
# test-v6-translator.rb
#
# Exercises the EBNF→treetop translator at scripts/lib/ebnf_to_treetop.rb
# against synthetic mini-grammars, with no dependency on the eventual
# tools/iso-19450-opl.ebnf pin file. Run directly:
#
#   bundle exec ruby tests/test-v6-translator.rb

require 'pathname'
require 'stringio'
require 'treetop'

REPO_ROOT = Pathname.new(__dir__).parent
require_relative '../scripts/lib/ebnf_to_treetop'

PASSED = []
FAILED = []

def assert_eq(actual, expected, label)
  if actual == expected
    PASSED << label
    print '.'
  else
    FAILED << label
    puts "\nFAIL: #{label}"
    puts "  expected: #{expected.inspect}"
    puts "  actual:   #{actual.inspect}"
  end
end

def assert_includes(haystack, needle, label)
  if haystack.include?(needle)
    PASSED << label
    print '.'
  else
    FAILED << label
    puts "\nFAIL: #{label}"
    puts "  expected to find: #{needle.inspect}"
    puts "  in:               #{haystack.inspect}"
  end
end

def assert_raises(label)
  yield
  FAILED << label
  puts "\nFAIL: #{label}: expected exception, got none"
rescue StandardError
  PASSED << label
  print '.'
end

# --- Lexer tests ---------------------------------------------------------

tokens = EBNFLexer.lex('foo = "bar" ;')
assert_eq(tokens, [[:ident, 'foo'], [:eq], [:terminal, '"bar"'], [:semi]],
          'lex: simple production')

tokens = EBNFLexer.lex("(* leading *) foo = 'x' ; (* trailing *)")
assert_eq(tokens, [[:ident, 'foo'], [:eq], [:terminal, "'x'"], [:semi]],
          'lex: comment stripping')

tokens = EBNFLexer.lex('foo = a , b | c ;')
assert_eq(tokens, [[:ident, 'foo'], [:eq], [:ident, 'a'], [:comma],
                   [:ident, 'b'], [:pipe], [:ident, 'c'], [:semi]],
          'lex: concat/alt operators')

tokens = EBNFLexer.lex('foo = [ a ] , { b } , ( c ) ;')
assert_eq(tokens.map(&:first),
          %i[ident eq lbrack ident rbrack comma lbrace ident rbrace comma lparen ident rparen semi],
          'lex: optional/repeat/group brackets')

tokens = EBNFLexer.lex('foo = letter - "e" ;')
assert_eq(tokens.find { |t| t[0] == :dash }, [:dash], 'lex: exception dash')

tokens = EBNFLexer.lex('foo = ? whitespace ? ;')
assert_eq(tokens.find { |t| t[0] == :special }, [:special, 'whitespace'],
          'lex: special sequence')

assert_raises('lex: unterminated terminal') { EBNFLexer.lex('foo = "unterminated ;') }
assert_raises('lex: unterminated comment')  { EBNFLexer.lex('foo (* never closed') }

# Hyphenated identifier
tokens = EBNFLexer.lex('object-declaration = "x" ;')
assert_eq(tokens.first, [:ident, 'object-declaration'], 'lex: hyphenated ident')

# --- Parser tests --------------------------------------------------------

prods = EBNFParser.new(EBNFLexer.lex('foo = "x" ;')).parse
assert_eq(prods, [['foo', [:terminal, '"x"']]], 'parse: simple terminal')

prods = EBNFParser.new(EBNFLexer.lex('foo = a , b , c ;')).parse
assert_eq(prods, [['foo', [:concat, [[:ref, 'a'], [:ref, 'b'], [:ref, 'c']]]]],
          'parse: 3-way concat')

prods = EBNFParser.new(EBNFLexer.lex('foo = a | b | c ;')).parse
assert_eq(prods, [['foo', [:alt, [[:ref, 'a'], [:ref, 'b'], [:ref, 'c']]]]],
          'parse: 3-way alt')

# alt has lower precedence than concat, so this is (a,b) | (c,d)
prods = EBNFParser.new(EBNFLexer.lex('foo = a , b | c , d ;')).parse
assert_eq(prods,
          [['foo',
            [:alt, [
              [:concat, [[:ref, 'a'], [:ref, 'b']]],
              [:concat, [[:ref, 'c'], [:ref, 'd']]]
            ]]]],
          'parse: alt-of-concats precedence')

prods = EBNFParser.new(EBNFLexer.lex('foo = [ a ] ;')).parse
assert_eq(prods, [['foo', [:optional, [:ref, 'a']]]], 'parse: optional')

prods = EBNFParser.new(EBNFLexer.lex('foo = { a } ;')).parse
assert_eq(prods, [['foo', [:repeat, [:ref, 'a']]]], 'parse: repeat')

prods = EBNFParser.new(EBNFLexer.lex('foo = ( a | b ) , c ;')).parse
assert_eq(prods,
          [['foo',
            [:concat, [
              [:group, [:alt, [[:ref, 'a'], [:ref, 'b']]]],
              [:ref, 'c']
            ]]]],
          'parse: grouping forces precedence')

prods = EBNFParser.new(EBNFLexer.lex('a = "x" ; b = "y" ;')).parse
assert_eq(prods.size, 2, 'parse: multiple productions')

# --- Emitter tests -------------------------------------------------------

src = TreetopEmitter.emit([['foo', [:terminal, '"x"']]])
assert_includes(src, "rule foo\n", 'emit: rule keyword')
assert_includes(src, '"x"', 'emit: terminal preserved')
assert_includes(src, 'def __rule__', 'emit: __rule__ tag method')
assert_includes(src, '"foo"', 'emit: __rule__ returns rulename')

src = TreetopEmitter.emit([['object-declaration', [:terminal, '"x"']]])
assert_includes(src, "rule object_declaration\n", 'emit: hyphen→underscore in rule name')
assert_includes(src, '"object_declaration"', 'emit: __rule__ uses sanitized name')

src = TreetopEmitter.emit([['foo', [:concat, [[:ref, 'a'], [:ref, 'b']]]]])
assert_includes(src, '(a b)', 'emit: concat as juxtaposition')

src = TreetopEmitter.emit([['foo', [:alt, [[:ref, 'a'], [:ref, 'b']]]]])
assert_includes(src, '(a / b)', 'emit: alt as ordered choice')

src = TreetopEmitter.emit([['foo', [:optional, [:ref, 'a']]]])
assert_includes(src, '(a)?', 'emit: optional as postfix ?')

src = TreetopEmitter.emit([['foo', [:repeat, [:ref, 'a']]]])
assert_includes(src, '(a)*', 'emit: repeat as postfix *')

# Exception emission (silence the warning for clean test output)
saved_err = $stderr
$stderr   = StringIO.new
src = TreetopEmitter.emit([['foo', [:exception, [:ref, 'a'], [:terminal, '"e"']]]])
$stderr   = saved_err
assert_includes(src, '(!("e") a)', 'emit: exception as PEG negative lookahead')

assert_raises('emit: special sequence raises') do
  TreetopEmitter.emit([['foo', [:special, 'whitespace']]])
end

# --- End-to-end: load and parse with the generated grammar ---------------

mini_ebnf = <<~EBNF
  greeting   = salutation , " " , subject , "." ;
  salutation = "Hello" | "Hi" ;
  subject    = "world" | "there" ;
EBNF

grammar_source = ebnf_to_treetop(mini_ebnf, grammar_name: 'MiniGreeting')
Treetop.load_from_string(grammar_source)
parser = MiniGreetingParser.new

['Hello world.', 'Hi there.', 'Hello there.'].each do |input|
  result = parser.parse(input)
  if result
    PASSED << "e2e parse #{input.inspect}"
    print '.'
    assert_eq(result.__rule__, 'greeting', "e2e: root __rule__ for #{input.inspect}")
  else
    FAILED << "e2e parse #{input.inspect}"
    puts "\nFAIL: e2e parse of #{input.inspect}: #{parser.failure_reason}"
  end
end

# Negative case
result = parser.parse('Howdy world.')
if result.nil?
  PASSED << 'e2e: bad input rejected'
  print '.'
else
  FAILED << 'e2e: bad input rejected'
  puts "\nFAIL: e2e expected parse to fail on bad salutation"
end

# Optional and repeat
mini_ebnf2 = <<~EBNF
  list = "[" , [ item , { "," , item } ] , "]" ;
  item = "a" | "b" | "c" ;
EBNF
grammar_source = ebnf_to_treetop(mini_ebnf2, grammar_name: 'MiniList')
Treetop.load_from_string(grammar_source)
parser2 = MiniListParser.new
['[]', '[a]', '[a,b,c]', '[b,b,b,a,c]'].each do |input|
  result = parser2.parse(input)
  if result
    PASSED << "e2e list #{input.inspect}"
    print '.'
  else
    FAILED << "e2e list #{input.inspect}"
    puts "\nFAIL: e2e list parse of #{input.inspect}: #{parser2.failure_reason}"
  end
end

# Walking the tree to find tagged nodes
mini_ebnf3 = <<~EBNF
  document = sentence , { sentence } ;
  sentence = subject , " is an object." ;
  subject  = "Foo" | "Bar" | "Baz" ;
EBNF
grammar_source = ebnf_to_treetop(mini_ebnf3, grammar_name: 'MiniDoc')
Treetop.load_from_string(grammar_source)
parser3 = MiniDocParser.new
result = parser3.parse('Foo is an object.Bar is an object.')
if result
  PASSED << 'e2e: doc parsed'
  print '.'
else
  FAILED << 'e2e: doc parsed'
  puts "\nFAIL: e2e doc parse: #{parser3.failure_reason}"
end

def walk(node, &blk)
  yield node
  return unless node.respond_to?(:elements) && node.elements
  node.elements.each { |e| walk(e, &blk) }
end

if result
  rule_counts = Hash.new(0)
  walk(result) do |node|
    rule_counts[node.__rule__] += 1 if node.respond_to?(:__rule__)
  end
  assert_eq(rule_counts['document'], 1, 'walk: 1 document node')
  assert_eq(rule_counts['sentence'], 2, 'walk: 2 sentence nodes')
  assert_eq(rule_counts['subject'],  2, 'walk: 2 subject nodes')
end

puts
puts "v6 translator tests: #{PASSED.size} passed, #{FAILED.size} failed"
exit(FAILED.empty? ? 0 : 1)
