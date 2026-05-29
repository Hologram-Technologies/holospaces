#!/usr/bin/env ruby
# frozen_string_literal: true
#
# v7-opd-opl-coherence.rb
#
# For each src/opm/<opd>/ subdirectory:
#   1. Parse opl.txt with the V6 EBNF parser. Walk the parse tree and
#      collect entity names per the production mappings in
#      tools/iso-19450-opd-coherence.txt.
#   2. Parse opd.svg as XML, extract the text-content set of every
#      <text> element.
#   3. Assert the two sets are equal (whitespace-trimmed). The OPL
#      set is the canonical source; any difference is a coherence
#      failure.
#
# Invoked by v7-opd-opl-coherence.sh after gap checks have confirmed
# both pin files and at least one opl.txt input exist.

require 'pathname'
require 'set'
require 'treetop'
require_relative 'lib/ebnf_to_treetop'
require_relative 'lib/opl_entity_extractor'

ebnf_path      = ARGV[0] or abort 'usage: v7-opd-opl-coherence.rb <ebnf> <coherence> <opm-dir>'
coherence_path = ARGV[1] or abort 'usage: v7-opd-opl-coherence.rb <ebnf> <coherence> <opm-dir>'
opm_dir        = ARGV[2] or abort 'usage: v7-opd-opl-coherence.rb <ebnf> <coherence> <opm-dir>'

EBNF      = Pathname.new(ebnf_path)
COHERENCE = Pathname.new(coherence_path)
OPM       = Pathname.new(opm_dir)

# --- Load V6 parser and coherence map ------------------------------------

grammar_source = ebnf_to_treetop(EBNF.read, grammar_name: 'OPL')
Treetop.load_from_string(grammar_source)
parser      = OPLParser.new
productions = OPLEntityExtractor.parse_coherence(COHERENCE.read)

# --- Per-OPD coherence check ---------------------------------------------

failed   = 0
opd_dirs = OPM.children.select(&:directory?).sort

opd_dirs.each do |opd|
  opl_file = opd / 'opl.txt'
  svg_file = opd / 'opd.svg'

  unless opl_file.exist? && svg_file.exist?
    failed += 1
    warn "V7: FAIL #{opd.basename}: missing opl.txt or opd.svg"
    next
  end

  parse_tree = parser.parse(opl_file.read)
  if parse_tree.nil?
    failed += 1
    warn "V7: FAIL #{opd.basename}: opl.txt did not parse against the V6 grammar"
    warn "  parse failure at line #{parser.failure_line}, column #{parser.failure_column}"
    warn "  rule: #{parser.failure_reason}"
    next
  end

  opl_entities = OPLEntityExtractor.entity_names(parse_tree, productions)
  svg_entities = OPLEntityExtractor.svg_entity_text(svg_file.read)

  only_in_opl = opl_entities - svg_entities
  only_in_svg = svg_entities - opl_entities
  next if only_in_opl.empty? && only_in_svg.empty?

  failed += 1
  warn "V7: FAIL #{opd.basename}"
  only_in_opl.each { |e| warn "  in OPL but not SVG: #{e}" }
  only_in_svg.each { |e| warn "  in SVG but not OPL: #{e}" }
end

if failed.positive?
  warn "V7: #{failed}/#{opd_dirs.size} OPD(s) failed coherence"
  exit 1
end

puts "V7: PASS (#{opd_dirs.size} OPD(s))"
