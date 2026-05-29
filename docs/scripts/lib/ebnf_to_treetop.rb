# frozen_string_literal: true
#
# scripts/lib/ebnf_to_treetop.rb
#
# Mechanical ISO/IEC 14977 EBNF → treetop PEG translator. Used by V6
# (parses OPL against tools/iso-19450-opl.ebnf) and by V7 (reuses V6's
# generated parser to walk OPL parse trees). Required by:
#
#   - scripts/v6-opl-syntax.rb
#   - scripts/v7-opd-opl-coherence.rb
#   - tests/test-v6-translator.rb
#
# The translator is intentionally mechanical: it transcribes one notation
# to another and does not encode correctness rules. If the EBNF contains a
# left-recursive production or one that exploits longest-match alternation
# semantics, the resulting PEG grammar will fail at parse time on those
# inputs. Resolution is a manual EBNF authoring decision documented in
# tools/iso-19450-opl.ebnf comments.
#
# Translation table (ISO/IEC 14977 EBNF → treetop PEG):
#
#   lhs = rhs ;            rule lhs<sanitized>; rhs ; end
#   a , b                  a b              (juxtaposition)
#   a | b                  a / b            (ordered choice — PEG)
#   [ a ]                  (a)?
#   { a }                  (a)*
#   ( a )                  (a)
#   "..." / '...'          "..." / '...'    (passthrough)
#   ? special ?            (no auto translation — raises)
#   a - b                  (!b a)           (PEG negative lookahead)
#   (* comment *)          stripped at lex time
#   identifier with `-`    hyphens become underscores
#
# Every emitted rule is tagged with `def __rule__; '<rulename>'; end` so
# V7 can identify a parse-tree node's originating production.

module EBNFLexer
  module_function

  # Lex EBNF text into a token stream. Tokens are arrays:
  #   [:ident, "name"], [:terminal, "\"foo\""], [:special, "content"],
  #   [:eq], [:semi], [:comma], [:pipe], [:lbrack], [:rbrack],
  #   [:lbrace], [:rbrace], [:lparen], [:rparen], [:dash].
  #
  # Comments `(* ... *)` are stripped during lex (not pre-pass) so that
  # `(*` appearing inside a terminal string is treated as part of the
  # terminal, not a comment opener.
  def lex(text)
    tokens = []
    i      = 0
    line   = 1
    col    = 1
    while i < text.length
      if text[i, 2] == '(*'
        j = text.index('*)', i + 2)
        raise "unterminated comment at line #{line}, column #{col}" unless j
        text[i...(j + 2)].each_char { |ch| ch == "\n" ? (line += 1; col = 1) : (col += 1) }
        i = j + 2
        next
      end
      c = text[i]
      case c
      when ' ', "\t", "\r"
        i += 1; col += 1
      when "\n"
        i += 1; line += 1; col = 1
      when '"', "'"
        j = text.index(c, i + 1)
        raise "unterminated terminal string at line #{line}, column #{col}" unless j
        tokens << [:terminal, text[i..j]]
        text[i..j].each_char { |ch| ch == "\n" ? (line += 1; col = 1) : (col += 1) }
        i = j + 1
      when '?'
        j = text.index('?', i + 1)
        raise "unterminated special sequence at line #{line}, column #{col}" unless j
        tokens << [:special, text[(i + 1)...j].strip]
        text[i..j].each_char { |ch| ch == "\n" ? (line += 1; col = 1) : (col += 1) }
        i = j + 1
      when '=' then tokens << [:eq];     i += 1; col += 1
      when ';' then tokens << [:semi];   i += 1; col += 1
      when ',' then tokens << [:comma];  i += 1; col += 1
      when '|' then tokens << [:pipe];   i += 1; col += 1
      when '[' then tokens << [:lbrack]; i += 1; col += 1
      when ']' then tokens << [:rbrack]; i += 1; col += 1
      when '{' then tokens << [:lbrace]; i += 1; col += 1
      when '}' then tokens << [:rbrace]; i += 1; col += 1
      when '(' then tokens << [:lparen]; i += 1; col += 1
      when ')' then tokens << [:rparen]; i += 1; col += 1
      when '-' then tokens << [:dash];   i += 1; col += 1
      else
        if c =~ /[A-Za-z]/
          j = i
          j += 1 while j < text.length && text[j] =~ /[A-Za-z0-9_-]/
          tokens << [:ident, text[i...j]]
          col += (j - i)
          i = j
        else
          raise "unexpected character #{c.inspect} at line #{line}, column #{col}"
        end
      end
    end
    tokens
  end
end

class EBNFParser
  def initialize(tokens)
    @tokens = tokens
    @pos    = 0
  end

  def parse
    productions = []
    productions << parse_production until at_end?
    productions
  end

  private

  def parse_production
    name = expect(:ident)
    expect(:eq)
    body = parse_alt
    expect(:semi)
    [name, body]
  end

  # alt := concat ('|' concat)*
  def parse_alt
    parts = [parse_concat]
    while peek == :pipe
      advance
      parts << parse_concat
    end
    parts.size == 1 ? parts[0] : [:alt, parts]
  end

  # concat := factor (',' factor)*
  def parse_concat
    parts = [parse_factor]
    while peek == :comma
      advance
      parts << parse_factor
    end
    parts.size == 1 ? parts[0] : [:concat, parts]
  end

  # factor := term ('-' term)?
  def parse_factor
    expr = parse_term
    if peek == :dash
      advance
      [:exception, expr, parse_term]
    else
      expr
    end
  end

  # term := identifier | terminal | special | '[' alt ']' | '{' alt '}' | '(' alt ')'
  def parse_term
    case peek
    when :ident    then [:ref, advance_value]
    when :terminal then [:terminal, advance_value]
    when :special  then [:special, advance_value]
    when :lbrack
      advance
      e = parse_alt
      expect(:rbrack)
      [:optional, e]
    when :lbrace
      advance
      e = parse_alt
      expect(:rbrace)
      [:repeat, e]
    when :lparen
      advance
      e = parse_alt
      expect(:rparen)
      [:group, e]
    else
      raise "unexpected token #{@tokens[@pos].inspect} at position #{@pos}"
    end
  end

  def at_end?
    @pos >= @tokens.length
  end

  def peek
    return nil if at_end?
    @tokens[@pos][0]
  end

  def advance
    tok = @tokens[@pos]
    @pos += 1
    tok
  end

  def advance_value
    tok = advance
    tok[1]
  end

  def expect(type)
    raise "expected #{type} but reached end of input" if at_end?
    tok = @tokens[@pos]
    unless tok[0] == type
      raise "expected #{type} but found #{tok.inspect} at position #{@pos}"
    end
    @pos += 1
    tok[1]
  end
end

module TreetopEmitter
  module_function

  RULE_NAME_RE = /\A[A-Za-z_][A-Za-z0-9_]*\z/

  def sanitize_name(name)
    s = name.tr('-', '_')
    raise "invalid rule identifier after sanitization: #{s.inspect}" unless s =~ RULE_NAME_RE
    s
  end

  # `grammar_name` is the module treetop creates. Treetop additionally
  # exposes a class named "<grammar_name>Parser" that callers instantiate.
  # E.g., grammar_name: 'OPL' → use OPLParser.new.
  def emit(productions, grammar_name: 'OPL')
    raise "invalid grammar name #{grammar_name.inspect}" unless grammar_name =~ /\A[A-Z][A-Za-z0-9_]*\z/

    rules = productions.map { |name, body| emit_rule(name, body) }
    "grammar #{grammar_name}\n#{rules.join("\n")}\nend\n"
  end

  def emit_rule(name, body)
    sanitized = sanitize_name(name)
    # Wrap the body in a labeled element. Without this, treetop inlines
    # a rule whose body is a single nonterminal reference: the outer
    # rule's match becomes the inner rule's syntax node and only the
    # inner rule's __rule__ tag survives. The label forces treetop to
    # produce a distinct outer node.
    "  rule #{sanitized}\n    __body__:(#{emit_node(body)})\n    {\n      def __rule__\n        #{sanitized.inspect}\n      end\n    }\n  end"
  end

  # Always parenthesise compound forms so that PEG precedence cannot
  # silently shift the meaning.
  def emit_node(node)
    case node[0]
    when :ref       then sanitize_name(node[1])
    when :terminal  then node[1]
    when :special   then handle_special(node[1])
    when :concat    then "(#{node[1].map { |n| emit_node(n) }.join(' ')})"
    when :alt       then "(#{node[1].map { |n| emit_node(n) }.join(' / ')})"
    when :optional  then "(#{emit_node(node[1])})?"
    when :repeat    then "(#{emit_node(node[1])})*"
    when :group     then "(#{emit_node(node[1])})"
    when :exception
      a, b = node[1], node[2]
      warn 'V6 translator: EBNF exception (a - b) translated to PEG ' \
           '(!b a) — verify semantic intent in tools/iso-19450-opl.ebnf'
      "(!(#{emit_node(b)}) #{emit_node(a)})"
    else
      raise "unknown EBNF AST node type: #{node[0]}"
    end
  end

  def handle_special(content)
    raise "EBNF special sequence ?#{content}? has no automatic translation. " \
          'Rewrite as a literal terminal or composite rule in tools/iso-19450-opl.ebnf.'
  end
end

# Top-level convenience entry point. Returns a treetop grammar source
# string ready for Treetop.load_from_string.
def ebnf_to_treetop(ebnf_text, grammar_name: 'OPL')
  tokens      = EBNFLexer.lex(ebnf_text)
  productions = EBNFParser.new(tokens).parse
  raise 'EBNF contains no productions' if productions.empty?
  TreetopEmitter.emit(productions, grammar_name: grammar_name)
end
