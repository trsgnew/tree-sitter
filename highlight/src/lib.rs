pub mod c_lib;
pub mod util;
pub use c_lib as c;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::{iter, mem, ops, str, usize};
use tree_sitter::{
    Language, Node, Parser, Point, Query, QueryCaptures, QueryCursor, QueryError, Range, Tree,
};

const CANCELLATION_CHECK_INTERVAL: usize = 100;

/// Indicates which highlight should be applied to a region of source code.
#[derive(Copy, Clone, Debug)]
pub struct Highlight(pub usize);

/// Represents the reason why syntax highlighting failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Cancelled,
    InvalidLanguage,
    Unknown,
}

/// Represents a single step in rendering a syntax-highlighted document.
#[derive(Copy, Clone, Debug)]
pub enum HighlightEvent {
    Source { start: usize, end: usize },
    HighlightStart(Highlight),
    HighlightEnd,
}

/// Contains the data neeeded to higlight code written in a particular language.
///
/// This struct is immutable and can be shared between threads.
pub struct HighlightConfiguration {
    pub language: Language,
    pub query: Query,
    injections_query: Query,
    locals_pattern_index: usize,
    highlights_pattern_index: usize,
    highlight_indices: Vec<Option<Highlight>>,
    non_local_variable_patterns: Vec<bool>,
    injection_site_capture_index: Option<u32>,
    injection_content_capture_index: Option<u32>,
    injection_language_capture_index: Option<u32>,
    local_scope_capture_index: Option<u32>,
    local_def_capture_index: Option<u32>,
    local_def_value_capture_index: Option<u32>,
    local_ref_capture_index: Option<u32>,
}

/// Performs syntax highlighting, recognizing a given list of highlight names.
///
/// Tree-sitter syntax-highlighting queries specify highlights in the form of dot-separated
/// highlight names like `punctuation.bracket` and `function.method.builtin`. Consumers of
/// these queries can choose to recognize highlights with different levels of specificity.
/// For example, the string `function.builtin` will match against `function.method.builtin`
/// and `function.builtin.constructor`, but will not match `function.method`.
///
/// The `Highlight` struct is instantiated with an ordered list of recognized highlight names
/// and is then used for loading highlight queries and performing syntax highlighting.
/// Highlighting results are returned as `Highlight` values, which contain the index of the
/// matched highlight this list of highlight names.
///
/// The `Highlighter` struct is immutable and can be shared between threads.
#[derive(Clone, Debug)]
pub struct Highlighter {
    highlight_names: Vec<String>,
}

/// Carries the mutable state required for syntax highlighting.
///
/// For the best performance `HighlightContext` values should be reused between
/// syntax highlighting calls. A separate context is needed for each thread that
/// is performing highlighting.
pub struct HighlightContext {
    parser: Parser,
    cursors: Vec<QueryCursor>,
}

/// Converts a general-purpose syntax highlighting iterator into a sequence of lines of HTML.
pub struct HtmlRenderer {
    pub html: Vec<u8>,
    pub line_offsets: Vec<u32>,
}

#[derive(Debug)]
struct LocalDef<'a> {
    name: &'a str,
    value_range: ops::Range<usize>,
    highlight: Option<Highlight>,
}

#[derive(Debug)]
struct LocalScope<'a> {
    inherits: bool,
    range: ops::Range<usize>,
    local_defs: Vec<LocalDef<'a>>,
}

struct HighlightIter<'a, F>
where
    F: Fn(&str) -> Option<&'a HighlightConfiguration> + 'a,
{
    source: &'a [u8],
    byte_offset: usize,
    context: &'a mut HighlightContext,
    injections_cursor: QueryCursor,
    injection_callback: F,
    cancellation_flag: Option<&'a AtomicUsize>,
    layers: Vec<HighlightIterLayer<'a>>,
    iter_count: usize,
    next_event: Option<HighlightEvent>,
    last_highlight_range: Option<(usize, usize, usize)>,
}

struct HighlightIterLayer<'a> {
    _tree: Tree,
    cursor: QueryCursor,
    captures: iter::Peekable<QueryCaptures<'a, &'a [u8]>>,
    config: &'a HighlightConfiguration,
    highlight_end_stack: Vec<usize>,
    scope_stack: Vec<LocalScope<'a>>,
    ranges: Vec<Range>,
    depth: usize,
}

impl HighlightContext {
    pub fn new() -> Self {
        HighlightContext {
            parser: Parser::new(),
            cursors: Vec::new(),
        }
    }
}

impl Highlighter {
    /// Creates a highlighter with a given list of recognized highlight names.
    pub fn new(highlight_names: Vec<String>) -> Self {
        Highlighter { highlight_names }
    }

    /// Returns the list of highlight names with which this Highlighter was constructed.
    pub fn names(&self) -> &[String] {
        &self.highlight_names
    }

    /// Creates a `HighlightConfiguration` for a given `Language` and set of highlighting
    /// queries.
    ///
    /// # Parameters
    ///
    /// * `language`  - The Tree-sitter `Language` that should be used for parsing.
    /// * `highlights_query` - A string containing tree patterns for syntax highlighting. This
    ///   should be non-empty, otherwise no syntax highlights will be added.
    /// * `injections_query` -  A string containing tree patterns for injecting other languages
    ///   into the document. This can be empty if no injections are desired.
    /// * `locals_query` - A string containing tree patterns for tracking local variable
    ///   definitions and references. This can be empty if local variable tracking is not needed.
    ///
    /// Returns a `HighlightConfiguration` that can then be used with the `highlight` method.
    pub fn load_configuration(
        &self,
        language: Language,
        highlights_query: &str,
        injection_query: &str,
        locals_query: &str,
    ) -> Result<HighlightConfiguration, QueryError> {
        // Concatenate the query strings, keeping track of the start offset of each section.
        let mut query_source = String::new();
        query_source.push_str(injection_query);
        let locals_query_offset = query_source.len();
        query_source.push_str(locals_query);
        let highlights_query_offset = query_source.len();
        query_source.push_str(highlights_query);

        // Construct a query with the concatenated string.
        let mut query = Query::new(language, &query_source)?;

        let injections_query = Query::new(language, injection_query)?;
        for injection_capture in injections_query.capture_names() {
            if injection_capture != "injection.site" {
                query.disable_capture(injection_capture);
            }
        }

        // Determine the range of pattern indices that belong to each section of the query.
        let mut locals_pattern_index = 0;
        let mut highlights_pattern_index = 0;
        for i in 0..(query.pattern_count()) {
            let pattern_offset = query.start_byte_for_pattern(i);
            if pattern_offset < highlights_query_offset {
                if pattern_offset < highlights_query_offset {
                    highlights_pattern_index += 1;
                }
                if pattern_offset < locals_query_offset {
                    locals_pattern_index += 1;
                }
            }
        }

        let mut capture_parts = Vec::new();

        // Compute a mapping from the query's capture ids to the indices of the highlighter's
        // recognized highlight names.
        let highlight_indices = query
            .capture_names()
            .iter()
            .map(move |capture_name| {
                capture_parts.clear();
                capture_parts.extend(capture_name.split('.'));

                let mut best_index = None;
                let mut best_match_len = 0;
                for (i, highlight_name) in self.highlight_names.iter().enumerate() {
                    let mut len = 0;
                    let mut matches = true;
                    for part in highlight_name.split('.') {
                        len += 1;
                        if !capture_parts.contains(&part) {
                            matches = false;
                            break;
                        }
                    }
                    if matches && len > best_match_len {
                        best_index = Some(i);
                        best_match_len = len;
                    }
                }
                best_index.map(Highlight)
            })
            .collect();

        let non_local_variable_patterns = (0..query.pattern_count())
            .map(|i| {
                query
                    .property_predicates(i)
                    .iter()
                    .any(|(prop, positive)| !*positive && prop.key.as_ref() == "local")
            })
            .collect();

        let mut injection_content_capture_index = None;
        let mut injection_language_capture_index = None;
        let mut injection_site_capture_index = None;
        let mut local_def_capture_index = None;
        let mut local_def_value_capture_index = None;
        let mut local_ref_capture_index = None;
        let mut local_scope_capture_index = None;
        for (i, name) in query.capture_names().iter().enumerate() {
            let i = Some(i as u32);
            match name.as_str() {
                "injection.content" => injection_content_capture_index = i,
                "injection.language" => injection_language_capture_index = i,
                "injection.site" => injection_site_capture_index = i,
                "local.definition" => local_def_capture_index = i,
                "local.definition-value" => local_def_value_capture_index = i,
                "local.reference" => local_ref_capture_index = i,
                "local.scope" => local_scope_capture_index = i,
                _ => {}
            }
        }

        Ok(HighlightConfiguration {
            language,
            query,
            injections_query,
            locals_pattern_index,
            highlights_pattern_index,
            highlight_indices,
            non_local_variable_patterns,
            injection_content_capture_index,
            injection_language_capture_index,
            injection_site_capture_index,
            local_def_capture_index,
            local_def_value_capture_index,
            local_ref_capture_index,
            local_scope_capture_index,
        })
    }

    /// Iterate over the highlighted regions for a given slice of source code.
    pub fn highlight<'a>(
        &'a self,
        context: &'a mut HighlightContext,
        config: &'a HighlightConfiguration,
        source: &'a [u8],
        cancellation_flag: Option<&'a AtomicUsize>,
        injection_callback: impl Fn(&str) -> Option<&'a HighlightConfiguration> + 'a,
    ) -> Result<impl Iterator<Item = Result<HighlightEvent, Error>> + 'a, Error> {
        let layer = HighlightIterLayer::new(
            config,
            source,
            context,
            cancellation_flag,
            0,
            vec![Range {
                start_byte: 0,
                end_byte: usize::MAX,
                start_point: Point::new(0, 0),
                end_point: Point::new(usize::MAX, usize::MAX),
            }],
        )?;

        let injections_cursor = context.cursors.pop().unwrap_or(QueryCursor::new());

        Ok(HighlightIter {
            source,
            byte_offset: 0,
            injection_callback,
            cancellation_flag,
            injections_cursor,
            context,
            iter_count: 0,
            layers: vec![layer],
            next_event: None,
            last_highlight_range: None,
        })
    }
}

impl<'a> HighlightIterLayer<'a> {
    fn new(
        config: &'a HighlightConfiguration,
        source: &'a [u8],
        context: &mut HighlightContext,
        cancellation_flag: Option<&'a AtomicUsize>,
        depth: usize,
        ranges: Vec<Range>,
    ) -> Result<Self, Error> {
        context
            .parser
            .set_language(config.language)
            .map_err(|_| Error::InvalidLanguage)?;
        unsafe { context.parser.set_cancellation_flag(cancellation_flag) };

        context.parser.set_included_ranges(&ranges);

        let tree = context.parser.parse(source, None).ok_or(Error::Cancelled)?;
        let mut cursor = context.cursors.pop().unwrap_or(QueryCursor::new());

        // The `captures` iterator borrows the `Tree` and the `QueryCursor`, which
        // prevents them from being moved. But both of these values are really just
        // pointers, so it's actually ok to move them.
        let tree_ref = unsafe { mem::transmute::<_, &'static Tree>(&tree) };
        let cursor_ref = unsafe { mem::transmute::<_, &'static mut QueryCursor>(&mut cursor) };
        let captures = cursor_ref
            .captures(&config.query, tree_ref.root_node(), move |n| {
                &source[n.byte_range()]
            })
            .peekable();

        Ok(HighlightIterLayer {
            highlight_end_stack: Vec::new(),
            scope_stack: vec![LocalScope {
                inherits: false,
                range: 0..usize::MAX,
                local_defs: Vec::new(),
            }],
            cursor,
            depth,
            _tree: tree,
            captures,
            config,
            ranges,
        })
    }

    // Compute the ranges that should be included when parsing an injection.
    // This takes into account three things:
    // * `parent_ranges` - The new injection may be nested inside of *another* injection
    //   (e.g. JavaScript within HTML within ERB). The parent injection's ranges must
    //   be taken into account.
    // * `nodes` - Every injection takes place within a set of nodes. The injection ranges
    //   are the ranges of those nodes.
    // * `includes_children` - For some injections, the content nodes' children should be
    //   excluded from the nested document, so that only the content nodes' *own* content
    //   is reparsed. For other injections, the content nodes' entire ranges should be
    //   reparsed, including the ranges of their children.
    fn intersect_ranges(&self, nodes: &Vec<Node>, includes_children: bool) -> Vec<Range> {
        let mut result = Vec::new();
        let mut parent_range_iter = self.ranges.iter();
        let mut parent_range = parent_range_iter
            .next()
            .expect("Layers should only be constructed with non-empty ranges vectors");
        for node in nodes.iter() {
            let mut preceding_range = Range {
                start_byte: 0,
                start_point: Point::new(0, 0),
                end_byte: node.start_byte(),
                end_point: node.start_position(),
            };
            let following_range = Range {
                start_byte: node.end_byte(),
                start_point: node.end_position(),
                end_byte: usize::MAX,
                end_point: Point::new(usize::MAX, usize::MAX),
            };

            for excluded_range in node
                .children()
                .filter_map(|child| {
                    if includes_children {
                        None
                    } else {
                        Some(child.range())
                    }
                })
                .chain([following_range].iter().cloned())
            {
                let mut range = Range {
                    start_byte: preceding_range.end_byte,
                    start_point: preceding_range.end_point,
                    end_byte: excluded_range.start_byte,
                    end_point: excluded_range.start_point,
                };
                preceding_range = excluded_range;

                if range.end_byte < parent_range.start_byte {
                    continue;
                }

                while parent_range.start_byte <= range.end_byte {
                    if parent_range.end_byte > range.start_byte {
                        if range.start_byte < parent_range.start_byte {
                            range.start_byte = parent_range.start_byte;
                            range.start_point = parent_range.start_point;
                        }

                        if parent_range.end_byte < range.end_byte {
                            if range.start_byte < parent_range.end_byte {
                                result.push(Range {
                                    start_byte: range.start_byte,
                                    start_point: range.start_point,
                                    end_byte: parent_range.end_byte,
                                    end_point: parent_range.end_point,
                                });
                            }
                            range.start_byte = parent_range.end_byte;
                            range.start_point = parent_range.end_point;
                        } else {
                            if range.start_byte < range.end_byte {
                                result.push(range);
                            }
                            break;
                        }
                    }

                    if let Some(next_range) = parent_range_iter.next() {
                        parent_range = next_range;
                    } else {
                        return result;
                    }
                }
            }
        }
        result
    }

    // First, sort scope boundaries by their byte offset in the document. At a
    // given position, emit scope endings before scope beginnings. Finally, emit
    // scope boundaries from deeper layers first.
    fn sort_key(&mut self) -> Option<(usize, bool, isize)> {
        let depth = -(self.depth as isize);
        let next_start = self
            .captures
            .peek()
            .map(|(m, i)| m.captures[*i].node.start_byte());
        let next_end = self.highlight_end_stack.last().cloned();
        match (next_start, next_end) {
            (Some(start), Some(end)) => {
                if start < end {
                    Some((start, true, depth))
                } else {
                    Some((end, false, depth))
                }
            }
            (Some(i), None) => Some((i, true, depth)),
            (None, Some(j)) => Some((j, false, depth)),
            _ => None,
        }
    }
}

impl<'a, F> HighlightIter<'a, F>
where
    F: Fn(&str) -> Option<&'a HighlightConfiguration> + 'a,
{
    fn emit_event(
        &mut self,
        offset: usize,
        event: Option<HighlightEvent>,
    ) -> Option<Result<HighlightEvent, Error>> {
        let result;
        if self.byte_offset < offset {
            result = Some(Ok(HighlightEvent::Source {
                start: self.byte_offset,
                end: offset,
            }));
            self.byte_offset = offset;
            self.next_event = event;
        } else {
            result = event.map(Ok);
        }
        self.sort_layers();
        result
    }

    fn sort_layers(&mut self) {
        if let Some(sort_key) = self.layers[0].sort_key() {
            let mut i = 0;
            while i + 1 < self.layers.len() {
                if let Some(next_offset) = self.layers[i + 1].sort_key() {
                    if next_offset < sort_key {
                        i += 1;
                        continue;
                    }
                }
                break;
            }
            if i > 0 {
                &self.layers[0..(i + 1)].rotate_left(1);
            }
        } else {
            let layer = self.layers.remove(0);
            self.context.cursors.push(layer.cursor);
        }
    }

    fn insert_layer(&mut self, mut layer: HighlightIterLayer<'a>) {
        let sort_key = layer.sort_key();
        let mut i = 1;
        while i < self.layers.len() {
            if self.layers[i].sort_key() > sort_key {
                self.layers.insert(i, layer);
                return;
            }
            i += 1;
        }
        self.layers.push(layer);
    }
}

impl<'a, F> Iterator for HighlightIter<'a, F>
where
    F: Fn(&str) -> Option<&'a HighlightConfiguration> + 'a,
{
    type Item = Result<HighlightEvent, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // If we've already determined the next highlight boundary, just return it.
            if let Some(e) = self.next_event.take() {
                return Some(Ok(e));
            }

            // Periodically check for cancellation, returning `Cancelled` error if the
            // cancellation flag was flipped.
            if let Some(cancellation_flag) = self.cancellation_flag {
                self.iter_count += 1;
                if self.iter_count >= CANCELLATION_CHECK_INTERVAL {
                    self.iter_count = 0;
                    if cancellation_flag.load(Ordering::Relaxed) != 0 {
                        return Some(Err(Error::Cancelled));
                    }
                }
            }

            // If none of the layers have any more scope boundaries, terminate.
            if self.layers.is_empty() {
                if self.byte_offset < self.source.len() {
                    let result = Some(Ok(HighlightEvent::Source {
                        start: self.byte_offset,
                        end: self.source.len(),
                    }));
                    self.byte_offset = self.source.len();
                    return result;
                } else {
                    return None;
                }
            }

            // Get the next capture. If there are no more captures, then emit the rest of the
            // source code.
            let match_;
            let mut captures;
            let mut capture;
            let mut pattern_index;
            let layer = &mut self.layers[0];
            if let Some((m, capture_index)) = layer.captures.peek() {
                match_ = m;
                captures = match_.captures;
                pattern_index = match_.pattern_index;
                capture = captures[*capture_index];
            } else if let Some(end_byte) = layer.highlight_end_stack.last().cloned() {
                layer.highlight_end_stack.pop();
                return self.emit_event(end_byte, Some(HighlightEvent::HighlightEnd));
            } else {
                return self.emit_event(self.source.len(), None);
            };

            // If any previous highlight ends before this node starts, then before
            // processing this capture, emit the source code up until the end of the
            // previous highlight, and an end event for that highlight.
            let range = capture.node.byte_range();
            if let Some(end_byte) = layer.highlight_end_stack.last().cloned() {
                if end_byte <= range.start {
                    layer.highlight_end_stack.pop();
                    return self.emit_event(end_byte, Some(HighlightEvent::HighlightEnd));
                }
            }

            // Remove from the scope stack any local scopes that have already ended.
            while range.start > layer.scope_stack.last().unwrap().range.end {
                layer.scope_stack.pop();
            }

            // If this capture represents an injection, then process the injection.
            if pattern_index < layer.config.locals_pattern_index {
                let site_capture_index = layer.config.injection_site_capture_index;
                let content_capture_index = layer.config.injection_content_capture_index;
                let language_capture_index = layer.config.injection_language_capture_index;

                // Injections must have a `injection.site` capture, which contains all of the
                // information about the injection.
                let site_node = match_.captures.iter().find_map(|c| {
                    if Some(c.index) == site_capture_index {
                        return Some(c.node);
                    } else {
                        return None;
                    }
                });

                // Explicitly remove this match so that none of its other captures will remain
                // in the stream of captures.
                layer.captures.next().unwrap().0.remove();

                if let Some(site_node) = site_node {
                    // Discard any subsequent matches for same injection site.
                    while let Some((next_match, _)) = layer.captures.peek() {
                        if next_match.pattern_index < layer.config.locals_pattern_index
                            && next_match
                                .captures
                                .iter()
                                .any(|c| Some(c.index) == site_capture_index && c.node == site_node)
                        {
                            layer.captures.next().unwrap().0.remove();
                            continue;
                        }
                        break;
                    }

                    // Find the language name and the nodes that represents the injection content.
                    // Use a separate Query and QueryCursor in order to avoid the injection
                    // captures being intermixed with other captures related to local variables
                    // and syntax highlighting.
                    let source = self.source;
                    let mut injections = Vec::<(usize, Option<&str>, Vec<Node>, bool)>::new();
                    for mat in self.injections_cursor.matches(
                        &layer.config.injections_query,
                        site_node,
                        move |node| &source[node.byte_range()],
                    ) {
                        let entry = if let Some(entry) =
                            injections.iter_mut().find(|e| e.0 == mat.pattern_index)
                        {
                            entry
                        } else {
                            injections.push((mat.pattern_index, None, Vec::new(), false));
                            injections.last_mut().unwrap()
                        };

                        for capture in mat.captures {
                            let index = Some(capture.index);
                            if index == site_capture_index {
                                if capture.node != site_node {
                                    break;
                                }
                            } else if index == language_capture_index && entry.1.is_none() {
                                entry.1 = capture.node.utf8_text(self.source).ok();
                            } else if index == content_capture_index {
                                entry.2.push(capture.node);
                            }
                        }
                    }

                    for (pattern_index, language, _, include_children) in injections.iter_mut() {
                        for prop in layer.config.query.property_settings(*pattern_index) {
                            match prop.key.as_ref() {
                                // In addition to specifying the language name via the text of a
                                // captured node, it can also be hard-coded via a `set!` predicate
                                // that sets the injection.language key.
                                "injection.language" => {
                                    if language.is_none() {
                                        *language = prop.value.as_ref().map(|s| s.as_ref())
                                    }
                                }

                                // By default, injections do not include the *children* of an
                                // `injection.content` node - only the ranges that belong to the
                                // node itself. This can be changed using a `set!` predicate that
                                // sets the `injection.include-children` key.
                                "injection.include-children" => *include_children = true,
                                _ => {}
                            }
                        }
                    }

                    for (_, language, content_nodes, include_children) in injections {
                        // If a language is found with the given name, then add a new language layer
                        // to the highlighted document.
                        if let Some(config) = language.and_then(&self.injection_callback) {
                            if !content_nodes.is_empty() {
                                match HighlightIterLayer::new(
                                    config,
                                    self.source,
                                    self.context,
                                    self.cancellation_flag,
                                    self.layers[0].depth + 1,
                                    self.layers[0]
                                        .intersect_ranges(&content_nodes, include_children),
                                ) {
                                    Ok(layer) => self.insert_layer(layer),
                                    Err(e) => return Some(Err(e)),
                                }
                            }
                        }
                    }

                    self.sort_layers();
                }

                continue;
            }

            layer.captures.next();

            // If this capture is for tracking local variables, then process the
            // local variable info.
            let mut reference_highlight = None;
            let mut definition_highlight = None;
            while pattern_index < layer.config.highlights_pattern_index {
                // If the node represents a local scope, push a new local scope onto
                // the scope stack.
                if Some(capture.index) == layer.config.local_scope_capture_index {
                    definition_highlight = None;
                    let mut scope = LocalScope {
                        inherits: true,
                        range: range.clone(),
                        local_defs: Vec::new(),
                    };
                    for prop in layer.config.query.property_settings(pattern_index) {
                        match prop.key.as_ref() {
                            "local.scope-inherits" => {
                                scope.inherits =
                                    prop.value.as_ref().map_or(true, |r| r.as_ref() == "true");
                            }
                            _ => {}
                        }
                    }
                    layer.scope_stack.push(scope);
                }
                // If the node represents a definition, add a new definition to the
                // local scope at the top of the scope stack.
                else if Some(capture.index) == layer.config.local_def_capture_index {
                    reference_highlight = None;
                    definition_highlight = None;
                    let scope = layer.scope_stack.last_mut().unwrap();

                    let mut value_range = 0..0;
                    for capture in captures {
                        if Some(capture.index) == layer.config.local_def_value_capture_index {
                            value_range = capture.node.byte_range();
                        }
                    }

                    if let Ok(name) = str::from_utf8(&self.source[range.clone()]) {
                        scope.local_defs.push(LocalDef {
                            name,
                            value_range,
                            highlight: None,
                        });
                        definition_highlight =
                            scope.local_defs.last_mut().map(|s| &mut s.highlight);
                    }
                }
                // If the node represents a reference, then try to find the corresponding
                // definition in the scope stack.
                else if Some(capture.index) == layer.config.local_ref_capture_index {
                    if definition_highlight.is_none() {
                        definition_highlight = None;
                        if let Ok(name) = str::from_utf8(&self.source[range.clone()]) {
                            for scope in layer.scope_stack.iter().rev() {
                                if let Some(highlight) =
                                    scope.local_defs.iter().rev().find_map(|def| {
                                        if def.name == name && range.start >= def.value_range.end {
                                            Some(def.highlight)
                                        } else {
                                            None
                                        }
                                    })
                                {
                                    reference_highlight = highlight;
                                    break;
                                }
                                if !scope.inherits {
                                    break;
                                }
                            }
                        }
                    }
                }

                // Continue processing any additional local-variable-tracking patterns
                // for the same node.
                if let Some((next_match, next_capture_index)) = layer.captures.peek() {
                    let next_capture = next_match.captures[*next_capture_index];
                    if next_capture.node == capture.node {
                        pattern_index = next_match.pattern_index;
                        captures = next_match.captures;
                        capture = next_capture;
                        layer.captures.next();
                        continue;
                    } else {
                        break;
                    }
                }

                break;
            }

            let mut has_highlight = true;
            if let Some((last_start, last_end, last_depth)) = self.last_highlight_range {
                if range.start == last_start && range.end == last_end && layer.depth < last_depth {
                    has_highlight = false;
                }
            }

            // If the current node was found to be a local variable, then skip over any
            // highlighting patterns that are disabled for local variables.
            while has_highlight
                && (definition_highlight.is_some() || reference_highlight.is_some())
                && layer.config.non_local_variable_patterns[pattern_index]
            {
                has_highlight = false;
                if let Some((next_match, next_capture_index)) = layer.captures.peek() {
                    let next_capture = next_match.captures[*next_capture_index];
                    if next_capture.node == capture.node {
                        capture = next_capture;
                        has_highlight = true;
                        pattern_index = next_match.pattern_index;
                        layer.captures.next();
                        continue;
                    }
                }
                break;
            }

            if has_highlight {
                // Once a highlighting pattern is found for the current node, skip over
                // any later highlighting patterns that also match this node. Captures
                // for a given node are ordered by pattern index, so these subsequent
                // captures are guaranteed to be for highlighting, not injections or
                // local variables.
                while let Some((next_match, next_capture_index)) = layer.captures.peek() {
                    if next_match.captures[*next_capture_index].node == capture.node {
                        layer.captures.next();
                    } else {
                        break;
                    }
                }

                let current_highlight = layer.config.highlight_indices[capture.index as usize];

                // If this node represents a local definition, then store the current
                // highlight value on the local scope entry representing this node.
                if let Some(definition_highlight) = definition_highlight {
                    *definition_highlight = current_highlight;
                }

                // Emit a scope start event and push the node's end position to the stack.
                if let Some(highlight) = reference_highlight.or(current_highlight) {
                    self.last_highlight_range = Some((range.start, range.end, layer.depth));
                    layer.highlight_end_stack.push(range.end);
                    return self
                        .emit_event(range.start, Some(HighlightEvent::HighlightStart(highlight)));
                }
            }

            self.sort_layers();
        }
    }
}

impl HtmlRenderer {
    pub fn new() -> Self {
        HtmlRenderer {
            html: Vec::new(),
            line_offsets: vec![0],
        }
    }

    pub fn reset(&mut self) {
        self.html.clear();
        self.line_offsets.clear();
        self.line_offsets.push(0);
    }

    pub fn render<'a, F>(
        &mut self,
        highlighter: impl Iterator<Item = Result<HighlightEvent, Error>>,
        source: &'a [u8],
        attribute_callback: &F,
    ) -> Result<(), Error>
    where
        F: Fn(Highlight) -> &'a [u8],
    {
        let mut highlights = Vec::new();
        for event in highlighter {
            match event {
                Ok(HighlightEvent::HighlightStart(s)) => {
                    highlights.push(s);
                    self.start_highlight(s, attribute_callback);
                }
                Ok(HighlightEvent::HighlightEnd) => {
                    highlights.pop();
                    self.end_highlight();
                }
                Ok(HighlightEvent::Source { start, end }) => {
                    self.add_text(&source[start..end], &highlights, attribute_callback);
                }
                Err(a) => return Err(a),
            }
        }
        if self.html.last() != Some(&b'\n') {
            self.html.push(b'\n');
        }
        if self.line_offsets.last() == Some(&(self.html.len() as u32)) {
            self.line_offsets.pop();
        }
        Ok(())
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.line_offsets
            .iter()
            .enumerate()
            .map(move |(i, line_start)| {
                let line_start = *line_start as usize;
                let line_end = if i + 1 == self.line_offsets.len() {
                    self.html.len()
                } else {
                    self.line_offsets[i + 1] as usize
                };
                str::from_utf8(&self.html[line_start..line_end]).unwrap()
            })
    }

    fn start_highlight<'a, F>(&mut self, h: Highlight, attribute_callback: &F)
    where
        F: Fn(Highlight) -> &'a [u8],
    {
        let attribute_string = (attribute_callback)(h);
        self.html.extend(b"<span");
        if !attribute_string.is_empty() {
            self.html.extend(b" ");
            self.html.extend(attribute_string);
        }
        self.html.extend(b">");
    }

    fn end_highlight(&mut self) {
        self.html.extend(b"</span>");
    }

    fn add_text<'a, F>(&mut self, src: &[u8], highlights: &Vec<Highlight>, attribute_callback: &F)
    where
        F: Fn(Highlight) -> &'a [u8],
    {
        for c in util::LossyUtf8::new(src).flat_map(|p| p.bytes()) {
            if c == b'\n' {
                if self.html.ends_with(b"\r") {
                    self.html.pop();
                }
                highlights.iter().for_each(|_| self.end_highlight());
                self.html.push(c);
                self.line_offsets.push(self.html.len() as u32);
                highlights
                    .iter()
                    .for_each(|scope| self.start_highlight(*scope, attribute_callback));
            } else if let Some(escape) = util::html_escape(c) {
                self.html.extend_from_slice(escape);
            } else {
                self.html.push(c);
            }
        }
    }
}
