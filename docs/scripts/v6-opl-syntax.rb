#!/usr/bin/env ruby
# frozen_string_literal: true
#
# v6-opl-syntax.rb
#
# Parses each src/opm/<opd>/opl.txt file against the pinned EBNF at
# tools/iso-19450-opl.ebnf via the treetop PEG parser. The script
# translates the EBNF to a treetop grammar at load time (mechanical
# EBNF→PEG translation, no rule rewriting); the EBNF file is the pin,
# the translation is deterministic.
#
# Invoked by v6-opl-syntax.sh after gap checks have confirmed both
# the EBNF pin file and at least one opl.txt input exist. Translator
# implementation lives at scripts/lib/ebnf_to_treetop.rb.

require 'pathname'
require 'treetop'
require_relative 'lib/ebnf_to_treetop'

ebnf_path = ARGV[0] or abort 'usage: v6-opl-syntax.rb <ebnf-path> <opm-dir>'
opm_dir   = ARGV[1] or abort 'usage: v6-opl-syntax.rb <ebnf-path> <opm-dir>'

EBNF = Pathname.new(ebnf_path)
OPM  = Pathname.new(opm_dir)

ebnf_text      = EBNF.read
grammar_source = ebnf_to_treetop(ebnf_text, grammar_name: 'OPL')
Treetop.load_from_string(grammar_source)
parser = OPLParser.new

failed    = 0
opl_files = OPM.glob('*/opl.txt').sort

opl_files.each do |opl|
  text   = opl.read
  result = parser.parse(text)
  if result.nil?
    failed += 1
    warn "V6: FAIL #{opl.relative_path_from(OPM.parent.parent)}"
    warn "  parse failure at line #{parser.failure_line}, " \
         "column #{parser.failure_column}"
    warn "  rule: #{parser.failure_reason}"
  end
end

if failed.positive?
  warn "V6: #{failed}/#{opl_files.size} OPL file(s) failed to parse"
  exit 1
end

puts "V6: PASS (#{opl_files.size} OPL file(s))"
