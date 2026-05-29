#!/usr/bin/env ruby
# frozen_string_literal: true
#
# v1-structural-alignment.rb
#
# Asserts that every chapter under src/arc42/adoc/ has the same H1 text and
# is a structural superset of the H2 set of the corresponding upstream
# chapter at vendor/arc42-generator/arc42-template/EN/asciidoc/src/.
#
# The "structure" being compared is what `Asciidoctor.load_file` parses;
# the local file may add extra H2 sections beyond the upstream set, but
# must not omit any.

require 'asciidoctor'
require 'pathname'

REPO_ROOT     = Pathname.new(File.expand_path('..', __dir__))
LOCAL_DIR     = REPO_ROOT / 'src' / 'arc42' / 'adoc'
UPSTREAM_DIR  = REPO_ROOT / 'vendor' / 'arc42-generator' / 'arc42-template' / 'EN' / 'adoc'

def headings_by_level(doc)
  by_level = Hash.new { |h, k| h[k] = [] }
  walk = lambda do |block|
    if block.respond_to?(:context) && block.context == :section
      by_level[block.level] << { text: block.title.to_s.strip,
                                 line: (block.source_location && block.source_location.lineno) }
    end
    block.blocks.each(&walk) if block.respond_to?(:blocks)
  end
  doc.blocks.each(&walk)
  by_level
end

def load_doc(path)
  Asciidoctor.load_file(path.to_s,
                        safe: :safe,
                        parse: true,
                        attributes: { 'skip-front-matter' => true })
end

unless LOCAL_DIR.directory?
  warn "V1: ERROR src/arc42/adoc/ does not exist"
  exit 2
end
unless UPSTREAM_DIR.directory?
  warn "V1: ERROR upstream chapters not found at #{UPSTREAM_DIR} (run: git submodule update --init --recursive)"
  exit 2
end

upstream_chapters = UPSTREAM_DIR.children
                                .select { |p| p.basename.to_s =~ /\A\d{2}_.+\.adoc\z/ }
                                .sort
violations = []

upstream_chapters.each do |upstream_path|
  filename = upstream_path.basename.to_s
  local_path = LOCAL_DIR / filename

  unless local_path.exist?
    violations << "missing local chapter: src/arc42/adoc/#{filename} (upstream: #{upstream_path.relative_path_from(REPO_ROOT)})"
    next
  end

  upstream_doc = load_doc(upstream_path)
  local_doc    = load_doc(local_path)

  upstream_h = headings_by_level(upstream_doc)
  local_h    = headings_by_level(local_doc)

  # Chapter files start at level-1 (==) since they are designed to be
  # included into a level-0 parent. Compare the FIRST level-1 heading
  # rather than doctitle (which is the level-0 = heading and is absent
  # from chapter files).
  upstream_h1 = (upstream_h[1].first && upstream_h[1].first[:text]) || ''
  local_h1    = (local_h[1].first    && local_h[1].first[:text])    || ''

  if upstream_h1 != local_h1
    violations << "H1 mismatch in #{filename}: upstream='#{upstream_h1}' local='#{local_h1}'"
  end

  upstream_h2_texts = upstream_h[2].map { |h| h[:text] }
  local_h2_texts    = local_h[2].map    { |h| h[:text] }
  missing = upstream_h2_texts - local_h2_texts
  missing.each do |t|
    upstream_line = upstream_h[2].find { |h| h[:text] == t }[:line]
    violations << "H2 missing in #{filename}: '#{t}' (upstream line #{upstream_line})"
  end
end

if violations.empty?
  puts "V1: PASS (#{upstream_chapters.size} chapters)"
  exit 0
else
  warn "V1: FAIL (#{violations.size} violation(s))"
  violations.each { |v| warn "  - #{v}" }
  exit 1
end
