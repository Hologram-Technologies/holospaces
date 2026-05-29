#!/usr/bin/env ruby
# frozen_string_literal: true
#
# tools/opl-to-svg.rb
#
# Bootstrap helper: generates an OPM-style SVG diagram from an OPL file.
# This is a developer aid, NOT part of the build pipeline. The committed
# `src/opm/<opd>/opd.svg` is a source artifact; V7 verifies coherence
# between it and the matching `opl.txt`. Run this script when adding a
# new OPD or after editing an OPL; the author may then hand-tune the
# SVG layout (which V7 still polices for entity-name coherence).
#
# Usage:
#   tools/opl-to-svg.rb <opl-path> [<svg-path>]
#
# If <svg-path> is omitted, writes to <opl-path>'s sibling `opd.svg`.
#
# Implementation:
#   1. Parse the OPL file into entities + relations.
#   2. Emit a Graphviz dot description with OPM-style shape conventions
#      (rectangles for objects, ellipses for processes, dashed borders
#      for environmental things).
#   3. Run `dot -Tsvg` to render the dot to SVG.
#   4. Post-process the SVG: every <text> element inside a <g class="node">
#      group is tagged with class="opm-entity" so V7 can identify it.
#      Edge labels (relation phrases) appear as plain <text> without the
#      class and are not counted by V7.
#
# Required: Graphviz (`dot`) on PATH. The wiki's install-tools.sh
# installs graphviz alongside its other apt deps.

require 'open3'
require 'pathname'
require 'rexml/document'
require 'set'

opl_path = ARGV[0] or abort 'usage: opl-to-svg.rb <opl-path> [<svg-path>]'
svg_path = ARGV[1] || (Pathname.new(opl_path).dirname + 'opd.svg').to_s

OPL = Pathname.new(opl_path).read

# --- Parse OPL -----------------------------------------------------------

declarations = []  # [name, :object|:process|:ambiguous, environmental?]
relations    = []  # [subject, predicate, object_or_target] (object_or_target may be nil for state lists)
process_subjects = Set.new

OPL.each_line do |raw|
  line = raw.strip
  next if line.empty?
  body = line.chomp('.').rstrip

  case body
  # --- Object/process declarations
  when /\A(.+?)\s+is\s+environmental\s+and\s+(physical|informatical)\z/
    name, essence = Regexp.last_match(1).strip, Regexp.last_match(2)
    declarations << [name, essence == 'physical' ? :object : :ambiguous, true]
  when /\A(.+?)\s+is\s+environmental\z/
    declarations << [Regexp.last_match(1).strip, :object, true]
  when /\A(.+?)\s+is\s+(physical|informatical)\z/
    name, essence = Regexp.last_match(1).strip, Regexp.last_match(2)
    declarations << [name, essence == 'physical' ? :object : :ambiguous, false]

  # --- Procedural links: process is the subject
  when /\A(.+?)\s+(yields|consumes|affects|requires|invokes)\s+(.+)\z/
    subj, verb, obj = Regexp.last_match(1).strip, Regexp.last_match(2), Regexp.last_match(3).strip
    relations << [subj, verb, obj]
    process_subjects << subj
  when /\A(.+?)\s+changes\s+(.+?)\s+from\s+(.+?)\s+to\s+(.+)\z/
    subj = Regexp.last_match(1).strip
    obj  = Regexp.last_match(2).strip
    s1   = Regexp.last_match(3).strip
    s2   = Regexp.last_match(4).strip
    relations << [subj, "changes from #{s1} to #{s2}", obj]
    process_subjects << subj

  # --- Agent: object handles process
  when /\A(.+?)\s+handles\s+(.+)\z/
    agent, process = Regexp.last_match(1).strip, Regexp.last_match(2).strip
    relations << [agent, 'handles', process]
    process_subjects << process

  # --- Structural: aggregation, exhibition
  when /\A(.+?)\s+consists\s+of\s+(.+)\z/
    whole, parts = Regexp.last_match(1).strip, Regexp.last_match(2).strip
    parts.split(/\s*,\s*and\s+|\s*,\s*|\s+and\s+/).each do |p|
      relations << [whole, 'consists of', p.strip]
    end
  when /\A(.+?)\s+exhibits\s+(.+)\z/
    bearer, features = Regexp.last_match(1).strip, Regexp.last_match(2).strip
    features.split(/\s*,\s*and\s+|\s*,\s*|\s+and\s+/).each do |f|
      relations << [bearer, 'exhibits', f.strip]
    end

  # --- Generalization, classification
  when /\A(.+?)\s+is\s+an\s+instance\s+of\s+(.+)\z/
    relations << [Regexp.last_match(1).strip, 'is an instance of', Regexp.last_match(2).strip]
  when /\A(.+?)\s+is\s+(?:a|an)\s+(.+)\z/
    relations << [Regexp.last_match(1).strip, 'is a', Regexp.last_match(2).strip]
  end
end

# Resolve :ambiguous declarations using process_subjects.
entities = declarations.map do |name, kind, env|
  [name, kind == :ambiguous ? (process_subjects.include?(name) ? :process : :object) : kind, env]
end

# --- Emit Graphviz dot ---------------------------------------------------

opd_name = Pathname.new(opl_path).dirname.basename.to_s

# Quote for dot's strict subset (escapes " and \).
def dot_quote(s)
  '"' + s.gsub('\\', '\\\\').gsub('"', '\\"') + '"'
end

dot_lines = []
dot_lines << "digraph #{dot_quote(opd_name)} {"
dot_lines << '  rankdir=LR;'
dot_lines << '  bgcolor="white";'
dot_lines << '  graph [pad="0.4", nodesep="0.6", ranksep="0.9"];'
dot_lines << '  node  [fontname="Helvetica", fontsize="11", penwidth="1.5"];'
dot_lines << '  edge  [fontname="Helvetica", fontsize="9", color="#1f2933", penwidth="1.0"];'

entities.each do |name, kind, env|
  shape = kind == :process ? 'ellipse' : 'box'
  fill  = kind == :process ? '#f4ddc7' : '#dce7f4'
  styles = ['filled']
  styles << 'dashed' if env
  attrs = [
    "shape=#{shape}",
    "fillcolor=\"#{fill}\"",
    "style=\"#{styles.join(',')}\"",
    'color="#1f2933"'
  ].join(', ')
  dot_lines << "  #{dot_quote(name)} [#{attrs}];"
end

# Procedural and structural arrowhead conventions:
#   - "is a" / "is an instance of"  → onormal (open triangle, OPM gen/cls)
#   - "consists of" / "exhibits"    → diamond (whole-side; we point at part)
#   - everything else (procedural)  → normal (default arrow)
relations.each do |subj, pred, obj|
  arrowhead =
    case pred
    when 'is a', 'is an instance of'           then 'onormal'
    when 'consists of', 'exhibits'             then 'odiamond'
    else                                            'normal'
    end
  attrs = [
    "label=#{dot_quote(pred)}",
    "arrowhead=#{arrowhead}"
  ].join(', ')
  dot_lines << "  #{dot_quote(subj)} -> #{dot_quote(obj)} [#{attrs}];"
end

dot_lines << '}'
dot_source = dot_lines.join("\n") + "\n"

# --- Render via dot ------------------------------------------------------

svg_raw, stderr_str, status =
  Open3.capture3('dot', '-Tsvg', stdin_data: dot_source)
unless status.success?
  warn "graphviz dot failed (#{status.exitstatus}):"
  warn stderr_str
  abort
end

# --- Post-process: tag entity-name <text> elements with class -----------
#
# Walk the SVG and add class="opm-entity" to every <text> inside
# <g class="node">. Edge labels (inside <g class="edge">) stay
# unclassed; V7 ignores them.

doc = REXML::Document.new(svg_raw)
doc.each_element('//g[@class="node"]/text') do |t|
  existing = t.attribute('class')
  if existing
    t.add_attribute('class', "#{existing.value} opm-entity")
  else
    t.add_attribute('class', 'opm-entity')
  end
end

# Drop the dot-emitted <title> elements inside node/edge groups
# (tooltips, not displayed, just clutter). REXML's delete_element
# removes one match per call, so iterate to drain.
loop do
  removed = doc.delete_element('//g/title')
  break if removed.nil?
end

# Write the SVG with stable formatting.
formatter = REXML::Formatters::Default.new
output = +''
formatter.write(doc, output)

File.write(svg_path, output)
warn "wrote #{svg_path} (#{entities.size} entities, #{relations.size} relations)"
