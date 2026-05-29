#!/usr/bin/env ruby
# frozen_string_literal: true
#
# test-v7-extractor.rb
#
# Unit tests for scripts/lib/opl_entity_extractor.rb plus an end-to-end
# pass over a synthetic mini-OPL grammar/coherence/SVG fixture set.
# No dependency on tools/iso-19450-opl.ebnf or tools/iso-19450-opd-coherence.txt.
#
# Run directly:
#
#   bundle exec ruby tests/test-v7-extractor.rb

require 'pathname'
require 'set'
require 'tmpdir'
require 'treetop'

REPO_ROOT = Pathname.new(__dir__).parent
require_relative '../scripts/lib/ebnf_to_treetop'
require_relative '../scripts/lib/opl_entity_extractor'

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

def assert_raises(label, pattern = nil)
  yield
  FAILED << label
  puts "\nFAIL: #{label}: expected exception, got none"
rescue StandardError => e
  if pattern && e.message !~ pattern
    FAILED << label
    puts "\nFAIL: #{label}: exception did not match #{pattern.inspect}"
    puts "  message: #{e.message}"
  else
    PASSED << label
    print '.'
  end
end

# --- parse_coherence -----------------------------------------------------

m = OPLEntityExtractor.parse_coherence(<<~COH)
  # comment
  object_declaration : entity_name
  process_declaration : entity_name

  state_declaration : state_name
COH
assert_eq(m, {
  'object_declaration'  => 'entity_name',
  'process_declaration' => 'entity_name',
  'state_declaration'   => 'state_name'
}, 'parse_coherence: 3-entry mapping with comment + blank lines')

# Hyphenated names get sanitized
m = OPLEntityExtractor.parse_coherence("object-declaration : entity-name\n")
assert_eq(m, { 'object_declaration' => 'entity_name' },
          'parse_coherence: hyphenated names sanitized')

assert_raises('parse_coherence: rejects malformed line', /invalid line/) do
  OPLEntityExtractor.parse_coherence("object_declaration entity_name\n")
end

assert_raises('parse_coherence: rejects empty file', /no production mappings/) do
  OPLEntityExtractor.parse_coherence("# only comments\n\n")
end

# --- svg_entity_text: filtering by class="opm-entity" --------------------

# Single-class text matches.
svg = <<~SVG
  <?xml version="1.0"?>
  <svg xmlns="http://www.w3.org/2000/svg">
    <text class="opm-entity">Foo</text>
    <text class="opm-entity">Bar</text>
  </svg>
SVG
assert_eq(OPLEntityExtractor.svg_entity_text(svg), Set['Foo', 'Bar'],
          'svg_entity_text: single-class entity texts collected')

# Edge labels (unclassed) ignored.
svg = <<~SVG
  <?xml version="1.0"?>
  <svg xmlns="http://www.w3.org/2000/svg">
    <text class="opm-entity">Foo</text>
    <text>yields</text>
    <text class="opm-entity">Bar</text>
    <text>handles</text>
  </svg>
SVG
assert_eq(OPLEntityExtractor.svg_entity_text(svg), Set['Foo', 'Bar'],
          'svg_entity_text: unclassed text (edge labels) ignored')

# Multi-class membership.
svg = <<~SVG
  <?xml version="1.0"?>
  <svg xmlns="http://www.w3.org/2000/svg">
    <text class="legend opm-entity highlighted">Foo</text>
    <text class="opm-entity">Bar</text>
  </svg>
SVG
assert_eq(OPLEntityExtractor.svg_entity_text(svg), Set['Foo', 'Bar'],
          'svg_entity_text: multi-class includes opm-entity token')

# Whitespace-trim and empty-text drop.
svg = <<~SVG
  <?xml version="1.0"?>
  <svg xmlns="http://www.w3.org/2000/svg">
    <text class="opm-entity">  Foo  </text>
    <text class="opm-entity"></text>
    <text class="opm-entity">Bar</text>
  </svg>
SVG
assert_eq(OPLEntityExtractor.svg_entity_text(svg), Set['Foo', 'Bar'],
          'svg_entity_text: whitespace-trim, empty-drop')

# --- entity_names: end-to-end with mini OPL grammar ----------------------

mini_opl = <<~EBNF
  document            = sentence , { sentence } ;
  sentence            = object_declaration | process_declaration | state_declaration ;
  object_declaration  = entity_name , " is an object." ;
  process_declaration = entity_name , " is a process." ;
  state_declaration   = state_name  , " is a state of " , entity_name , "." ;
  entity_name         = "Foo" | "Bar" | "Baz" | "Qux" | "System" ;
  state_name          = "Idle" | "Running" | "Done" ;
EBNF

grammar_source = ebnf_to_treetop(mini_opl, grammar_name: 'MiniOPL')
Treetop.load_from_string(grammar_source)
parser = MiniOPLParser.new

coherence = OPLEntityExtractor.parse_coherence(<<~COH)
  object_declaration : entity_name
  process_declaration : entity_name
  state_declaration : state_name
COH

# Test 1: just objects
tree = parser.parse('Foo is an object.Bar is an object.')
raise 'mini-OPL parse 1 failed' if tree.nil?
entities = OPLEntityExtractor.entity_names(tree, coherence)
assert_eq(entities, Set['Foo', 'Bar'], 'entity_names: object-only OPL')

# Test 2: mixed
tree = parser.parse('Foo is an object.Bar is a process.Baz is an object.')
raise 'mini-OPL parse 2 failed' if tree.nil?
entities = OPLEntityExtractor.entity_names(tree, coherence)
assert_eq(entities, Set['Foo', 'Bar', 'Baz'],
          'entity_names: mixed object/process declarations')

# Test 3: state declarations — state_declaration introduces the STATE name,
# not the entity-name (which is just a reference). The coherence map says
# state_declaration : state_name, so only the state name is extracted.
tree = parser.parse('Foo is an object.Idle is a state of Foo.')
raise 'mini-OPL parse 3 failed' if tree.nil?
entities = OPLEntityExtractor.entity_names(tree, coherence)
assert_eq(entities, Set['Foo', 'Idle'],
          'entity_names: state_declaration extracts state_name not entity_name')

# Test 4: dedup — same entity declared twice
tree = parser.parse('Foo is an object.Foo is an object.')
raise 'mini-OPL parse 4 failed' if tree.nil?
entities = OPLEntityExtractor.entity_names(tree, coherence)
assert_eq(entities, Set['Foo'], 'entity_names: duplicate declarations dedup')

# Test 5: empty parse tree (just one sentence)
tree = parser.parse('Qux is a process.')
raise 'mini-OPL parse 5 failed' if tree.nil?
entities = OPLEntityExtractor.entity_names(tree, coherence)
assert_eq(entities, Set['Qux'], 'entity_names: single-sentence document')

# --- End-to-end V7 invocation against synthetic fixture -----------------
#
# Build a temp pin-file/source-tree set, invoke v7-opd-opl-coherence.rb,
# and verify it exits 0 (coherent) or 1 (drift) as expected.

V7_SCRIPT = REPO_ROOT / 'scripts' / 'v7-opd-opl-coherence.rb'

def run_v7(ebnf, coherence, opm_dir)
  out_file = "#{opm_dir}/v7.out"
  err_file = "#{opm_dir}/v7.err"
  rc = system("bundle exec ruby #{V7_SCRIPT} #{ebnf} #{coherence} #{opm_dir} " \
              ">#{out_file} 2>#{err_file}")
  status = $?.exitstatus
  [status, File.read(out_file), File.read(err_file)]
end

Dir.mktmpdir('v7-fixture') do |tmp|
  ebnf_path      = "#{tmp}/grammar.ebnf"
  coherence_path = "#{tmp}/coherence.txt"
  opm_path       = "#{tmp}/opm"
  Dir.mkdir(opm_path)

  File.write(ebnf_path, mini_opl)
  File.write(coherence_path, <<~COH)
    object_declaration : entity_name
    process_declaration : entity_name
  COH

  # Coherent OPD: opl.txt declares Foo and Bar; svg labels Foo and Bar
  # via class="opm-entity" text elements. An unclassed <text> (the
  # "yields" edge label) is included to verify V7 ignores it.
  Dir.mkdir("#{opm_path}/opd1")
  File.write("#{opm_path}/opd1/opl.txt", 'Foo is an object.Bar is a process.')
  File.write("#{opm_path}/opd1/opd.svg", <<~SVG)
    <?xml version="1.0"?>
    <svg xmlns="http://www.w3.org/2000/svg">
      <text class="opm-entity">Foo</text>
      <text class="opm-entity">Bar</text>
      <text>yields</text>
    </svg>
  SVG

  status, _stdout, stderr = run_v7(ebnf_path, coherence_path, opm_path)
  if status.zero?
    PASSED << 'e2e: coherent OPD passes V7'
    print '.'
  else
    FAILED << 'e2e: coherent OPD passes V7'
    puts "\nFAIL: e2e coherent OPD: V7 exited #{status}"
    puts "  stderr: #{stderr}"
  end

  # Drifted OPD: opl.txt declares Foo and Baz; svg labels Foo and Bar
  # (Baz only in OPL, Bar only in SVG). Edge label "yields" is again
  # present without the class and must not affect the comparison.
  Dir.mkdir("#{opm_path}/opd2")
  File.write("#{opm_path}/opd2/opl.txt", 'Foo is an object.Baz is a process.')
  File.write("#{opm_path}/opd2/opd.svg", <<~SVG)
    <?xml version="1.0"?>
    <svg xmlns="http://www.w3.org/2000/svg">
      <text class="opm-entity">Foo</text>
      <text class="opm-entity">Bar</text>
      <text>yields</text>
    </svg>
  SVG

  status, _stdout, stderr = run_v7(ebnf_path, coherence_path, opm_path)
  if status == 1 && stderr.include?('Baz') && stderr.include?('Bar')
    PASSED << 'e2e: drifted OPD fails V7 with symmetric difference'
    print '.'
  else
    FAILED << 'e2e: drifted OPD fails V7 with symmetric difference'
    puts "\nFAIL: e2e drifted OPD: V7 exited #{status}, stderr did not name both drifted entities"
    puts "  stderr: #{stderr}"
  end
end

puts
puts "v7 extractor tests: #{PASSED.size} passed, #{FAILED.size} failed"
exit(FAILED.empty? ? 0 : 1)
