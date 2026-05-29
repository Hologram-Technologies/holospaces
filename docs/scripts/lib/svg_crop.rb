# frozen_string_literal: true
#
# scripts/lib/svg_crop.rb
#
# Crops an SVG's viewBox + width + height to a tight bounding box around
# its visible content, with configurable padding. Required because the
# Structurizr SVG exporter emits a fixed canvas size per view (e.g.,
# 2000x2000 for the deployment views) regardless of how much room the
# autoLayout actually consumed; tightened layouts produce small content
# in the top-left corner of an oversized canvas, rendering as
# "jumbled boxes in the corner of the frame" on the wiki.
#
# This cropper walks the SVG tree, accumulates transform matrices,
# computes the bounding box of every visible primitive (`<rect>`,
# `<ellipse>`, `<circle>`, `<polygon>`, `<polyline>`, and visible
# `<text>` positions), and rewrites the SVG's `viewBox`, `width`, and
# `height` attributes to that box plus padding.
#
# Required by:
#   - scripts/v3-structurizr.sh (Phase 2.5 post-processing)
#
# Conservative by design:
#   - Skips `<path>` (its `d` attribute requires a full path parser);
#     paths used by Structurizr are edge connectors that travel
#     between two nodes, so the box bounds derived from rects/ellipses
#     subsume them in practice.
#   - Skips elements with `display="none"`, `visibility="hidden"`, or
#     a `style=` attribute setting either of those.
#   - Treats `<text>` as a point at its (x, y); doesn't try to predict
#     glyph extent. Padding compensates.
#
# Usage:
#   require_relative 'scripts/lib/svg_crop'
#   SvgCrop.crop_file('images/c4-deployment-application.svg', padding: 24)

require 'rexml/document'

module SvgCrop
  module_function

  DEFAULT_PADDING = 24

  # Crop the SVG file in place. Returns the new viewBox as a string.
  def crop_file(path, padding: DEFAULT_PADDING)
    doc      = REXML::Document.new(File.read(path))
    bbox     = compute_bbox(doc.root, identity_matrix)
    return nil if bbox.nil?

    min_x, min_y, max_x, max_y = bbox
    new_x       = (min_x - padding).floor
    new_y       = (min_y - padding).floor
    new_width   = (max_x - min_x + 2 * padding).ceil
    new_height  = (max_y - min_y + 2 * padding).ceil

    doc.root.add_attribute('viewBox', "#{new_x} #{new_y} #{new_width} #{new_height}")
    doc.root.add_attribute('width', new_width.to_s)
    doc.root.add_attribute('height', new_height.to_s)
    # Keep an inline style consistent with the existing structurizr output.
    doc.root.add_attribute('style', "width: #{new_width}px; height: #{new_height}px; background: #ffffff")

    formatter = REXML::Formatters::Default.new
    out = +''
    formatter.write(doc, out)
    File.write(path, out)
    "#{new_x} #{new_y} #{new_width} #{new_height}"
  end

  # --- Affine matrix helpers ---------------------------------------------
  #
  # 2D affine matrix represented as [a, b, c, d, e, f] where a point
  # (x, y) maps to (a*x + c*y + e, b*x + d*y + f).

  def identity_matrix
    [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
  end

  def multiply(m, n)
    a, b, c, d, e, f = m
    a2, b2, c2, d2, e2, f2 = n
    [
      a * a2 + c * b2,
      b * a2 + d * b2,
      a * c2 + c * d2,
      b * c2 + d * d2,
      a * e2 + c * f2 + e,
      b * e2 + d * f2 + f
    ]
  end

  def apply(m, x, y)
    a, b, c, d, e, f = m
    [a * x + c * y + e, b * x + d * y + f]
  end

  def parse_transform(str)
    return identity_matrix if str.nil? || str.strip.empty?
    m = identity_matrix
    str.scan(/(\w+)\s*\(\s*([^)]+)\s*\)/) do |op, args_str|
      args = args_str.split(/[,\s]+/).reject(&:empty?).map(&:to_f)
      part = case op
             when 'translate'
               tx, ty = args[0] || 0.0, args[1] || 0.0
               [1.0, 0.0, 0.0, 1.0, tx, ty]
             when 'scale'
               sx = args[0] || 1.0
               sy = args[1] || sx
               [sx, 0.0, 0.0, sy, 0.0, 0.0]
             when 'matrix'
               args + [0.0] * (6 - args.size)
             when 'rotate'
               # Skip rotation in bounding-box math; Structurizr SVG
               # output does not appear to use it for visible primitives.
               identity_matrix
             else
               identity_matrix
             end
      m = multiply(m, part)
    end
    m
  end

  # --- Bounding box accumulation -----------------------------------------

  def compute_bbox(node, parent_matrix)
    return nil unless node.is_a?(REXML::Element)
    return nil if hidden?(node)

    own_transform = parse_transform(node.attribute('transform')&.value)
    matrix = multiply(parent_matrix, own_transform)

    bbox = primitive_bbox(node, matrix)

    node.elements.each do |child|
      child_bbox = compute_bbox(child, matrix)
      bbox = union(bbox, child_bbox)
    end
    bbox
  end

  def hidden?(el)
    return true if el.attribute('display')&.value == 'none'
    return true if el.attribute('visibility')&.value == 'hidden'
    style = el.attribute('style')&.value || ''
    return true if style =~ /display\s*:\s*none/
    return true if style =~ /visibility\s*:\s*hidden/
    false
  end

  def primitive_bbox(el, m)
    case el.name
    when 'rect' then rect_bbox(el, m)
    when 'ellipse' then ellipse_bbox(el, m)
    when 'circle' then circle_bbox(el, m)
    when 'polygon', 'polyline' then polyline_bbox(el, m)
    when 'text' then text_bbox(el, m)
    else nil
    end
  end

  def rect_bbox(el, m)
    x = (el.attribute('x')&.value || '0').to_f
    y = (el.attribute('y')&.value || '0').to_f
    w = (el.attribute('width')&.value || '0').to_f
    h = (el.attribute('height')&.value || '0').to_f
    return nil if w <= 0 || h <= 0
    corners = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]
    points_bbox(corners.map { |px, py| apply(m, px, py) })
  end

  def ellipse_bbox(el, m)
    cx = (el.attribute('cx')&.value || '0').to_f
    cy = (el.attribute('cy')&.value || '0').to_f
    rx = (el.attribute('rx')&.value || '0').to_f
    ry = (el.attribute('ry')&.value || '0').to_f
    return nil if rx <= 0 || ry <= 0
    corners = [[cx - rx, cy - ry], [cx + rx, cy - ry], [cx + rx, cy + ry], [cx - rx, cy + ry]]
    points_bbox(corners.map { |px, py| apply(m, px, py) })
  end

  def circle_bbox(el, m)
    cx = (el.attribute('cx')&.value || '0').to_f
    cy = (el.attribute('cy')&.value || '0').to_f
    r  = (el.attribute('r')&.value  || '0').to_f
    return nil if r <= 0
    corners = [[cx - r, cy - r], [cx + r, cy - r], [cx + r, cy + r], [cx - r, cy + r]]
    points_bbox(corners.map { |px, py| apply(m, px, py) })
  end

  def polyline_bbox(el, m)
    pts_str = el.attribute('points')&.value || ''
    coords  = pts_str.split(/[,\s]+/).reject(&:empty?).map(&:to_f)
    return nil if coords.size < 4
    points = coords.each_slice(2).to_a.map { |px, py| apply(m, px, py) }
    points_bbox(points)
  end

  def text_bbox(el, m)
    x = (el.attribute('x')&.value || '0').to_f
    y = (el.attribute('y')&.value || '0').to_f
    px, py = apply(m, x, y)
    [px, py, px, py]
  end

  def points_bbox(points)
    xs = points.map(&:first)
    ys = points.map(&:last)
    [xs.min, ys.min, xs.max, ys.max]
  end

  def union(a, b)
    return b if a.nil?
    return a if b.nil?
    [
      [a[0], b[0]].min,
      [a[1], b[1]].min,
      [a[2], b[2]].max,
      [a[3], b[3]].max
    ]
  end
end
