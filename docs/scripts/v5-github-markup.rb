#!/usr/bin/env ruby
# frozen_string_literal: true
#
# v5-github-markup.rb
#
# Calls GitHub::Markup.render(filename, content) for each .md file passed
# on ARGV. Exits non-zero if any invocation raises or returns empty/nil.
#
# Scope reminder: covers only the markup-to-HTML stage GitHub itself runs.
# Downstream sanitization, syntax highlighting, emoji, task list, and CDN
# image rewriting are NOT covered (they live server-side at github.com).

require 'github/markup'

failed = 0
ARGV.each do |path|
  begin
    content = File.read(path)
    html = GitHub::Markup.render(path, content)
    if html.nil? || html.strip.empty?
      warn "V5: FAIL #{path} — render returned empty/nil"
      failed += 1
    end
  rescue => e
    warn "V5: FAIL #{path} — #{e.class}: #{e.message}"
    e.backtrace.first(5).each { |line| warn "    #{line}" }
    failed += 1
  end
end

if failed.positive?
  warn "V5: #{failed} file(s) failed"
  exit 1
end
puts "V5: PASS (#{ARGV.size} file(s))"
