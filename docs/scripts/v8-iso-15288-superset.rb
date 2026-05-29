#!/usr/bin/env ruby
# frozen_string_literal: true
#
# v8-iso-15288-superset.rb
#
# Walks each src/15288/*.adoc file's AST via Asciidoctor, collects
# section headings, asserts that every process named in
# tools/iso-15288-processes.txt appears as a heading in the
# corresponding process-group .adoc file.
#
# The pin file format: blank lines and lines starting with `#` are
# ignored. Each non-comment line is either a process-group header
# of the form `[<group>]` (where <group> matches the .adoc filename
# stem under src/15288/) or a process name. Process names following
# a header belong to that group. Example:
#
#   [agreement]
#   Acquisition
#   Supply
#
#   [technical]
#   Business or Mission Analysis
#   Stakeholder Needs and Requirements Definition

require 'pathname'
require 'asciidoctor'

processes_path = ARGV[0] or abort 'usage: v8-iso-15288-superset.rb <processes-file> <lifecycle-dir>'
lifecycle_dir  = ARGV[1] or abort 'usage: v8-iso-15288-superset.rb <processes-file> <lifecycle-dir>'

PROCESSES = Pathname.new(processes_path)
LIFECYCLE = Pathname.new(lifecycle_dir)

# --- Parse the pinned process catalog ------------------------------------

def parse_processes(text)
  groups = {}
  current = nil
  text.each_line do |raw|
    line = raw.strip
    next if line.empty? || line.start_with?('#')
    if (m = line.match(/\A\[([a-z0-9-]+)\]\z/))
      current = m[1]
      groups[current] ||= []
    else
      raise "process name outside any [group] header: #{line.inspect}" if current.nil?
      groups[current] << line
    end
  end
  groups
end

# --- Collect section headings from an .adoc file -------------------------

def section_headings(adoc_path)
  doc = Asciidoctor.load_file(adoc_path.to_s, safe: :safe, parse: true)
  headings = []
  walk = lambda do |block|
    if block.respond_to?(:context) && block.context == :section
      headings << block.title.to_s.strip
    end
    block.blocks.each(&walk) if block.respond_to?(:blocks)
  end
  doc.blocks.each(&walk)
  headings
end

# --- Per-group structural superset check ---------------------------------

groups = parse_processes(PROCESSES.read)
missing_total = 0
checked_groups = 0

groups.each do |group, expected_processes|
  adoc_file = LIFECYCLE / "#{group}.adoc"
  unless adoc_file.exist?
    warn "V8: FAIL group #{group}: missing #{adoc_file.relative_path_from(LIFECYCLE.parent.parent)}"
    missing_total += expected_processes.size
    next
  end
  checked_groups += 1
  actual_headings = section_headings(adoc_file)
  expected_processes.each do |proc_name|
    unless actual_headings.include?(proc_name)
      missing_total += 1
      warn "V8: FAIL #{group}.adoc missing process heading: #{proc_name.inspect}"
    end
  end
end

if missing_total.positive?
  warn "V8: #{missing_total} process heading(s) missing across #{groups.size} group(s)"
  exit 1
end

puts "V8: PASS (#{checked_groups}/#{groups.size} groups, #{groups.values.flatten.size} processes)"
