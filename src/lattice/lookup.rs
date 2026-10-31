use std::ops::Range;

use super::{
    Candidate, CharInfo, Dictionary, Error, HitSite, Lattice, LinkSpan, NodeSource,
    PackedDictionary, PackedView, PrefixHit, RawNode, Span, SystemDictionary, UserIndex,
};

struct UnknownSite {
    begin: usize,
    surface: usize,
    end: usize,
    whole: bool,
    char_info: CharInfo,
    width: usize,
}

impl UnknownSite {
    #[inline(always)]
    fn span(&self, end: usize) -> Span {
        Span::new(self.begin, self.surface, end)
    }
}

#[inline(always)]
fn hit_ends(
    dictionary: &Dictionary,
    sentence: &[u8],
    found: &HitSite,
    hit: PrefixHit,
) -> (usize, usize) {
    let hit_end = found.surface + hit.length();
    let end = dictionary
        .chars
        .extended_end(sentence, hit_end, found.limit);
    (hit_end, end)
}

impl Lattice {
    #[inline(always)]
    pub(super) fn lookup<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        system: &PackedDictionary,
        view: &PackedView<'_>,
        sentence: &[u8],
        begin: usize,
    ) -> Result<Range<usize>, Error> {
        let chars = &dictionary.chars;
        let sentence_len = sentence.len();
        let first = self.node_count::<BEST>();
        let partial = !BEST && self.constraints.is_active();
        let end = self.partial_end::<BEST>(sentence_len, begin);
        let whole = end == sentence_len;
        let (surface, scan) = if whole {
            self.sentence_chars.skip_space(begin, chars.space())
        } else {
            chars.skip_space(sentence, begin, end)
        };
        if surface == sentence_len {
            return Ok(first..first);
        }
        let char_info = scan.info;
        let width = if surface == end {
            chars.char_info(sentence, surface, sentence_len).1
        } else {
            scan.width
        };
        let key_end = if end == surface { sentence_len } else { end };
        let found = HitSite {
            begin,
            surface,
            partial,
            char_type: char_info.default_type() as u8,
            limit: end,
            key_end,
            whole,
        };
        self.add_dictionary_hits::<BEST>(dictionary, system, view, sentence, &found);
        if self.node_count::<BEST>() > first && !char_info.invoke() {
            return Ok(first..self.node_count::<BEST>());
        }
        let site = UnknownSite {
            begin,
            surface,
            end,
            whole,
            char_info,
            width,
        };
        self.add_unknown_candidates::<BEST>(dictionary, system, sentence, &site, first);
        if partial && self.node_count::<BEST>() == first {
            self.add_constrained_unknown(dictionary, system, sentence, begin, surface, end)?;
        }
        Ok(first..self.node_count::<BEST>())
    }

    #[inline(always)]
    fn partial_end<const BEST: bool>(&self, sentence_len: usize, begin: usize) -> usize {
        if !BEST && self.constraints.is_active() {
            return self.constraints.next_token_boundary(begin);
        }
        sentence_len
    }

    #[inline(always)]
    fn add_dictionary_hits<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        system: &PackedDictionary,
        view: &PackedView<'_>,
        sentence: &[u8],
        found: &HitSite,
    ) {
        let key = &sentence[found.surface..found.key_end];
        if found.whole {
            view.common_prefix_search_sentence(&self.sentence_chars, found.surface, &mut self.hits);
        } else {
            view.common_prefix_search(key, &mut self.hits);
        }
        self.push_packed_hits::<BEST>(dictionary, system, view, sentence, found);
        for (index, user) in dictionary.users.iter().enumerate() {
            user.common_prefix_search(key, &mut self.hits);
            let source = NodeSource::User(UserIndex::from_position(index));
            self.push_system_hits::<BEST>(dictionary, user, source, sentence, found);
        }
    }

    #[inline(always)]
    fn add_unknown_candidates<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        unknown: &PackedDictionary,
        sentence: &[u8],
        site: &UnknownSite,
        first: usize,
    ) {
        let after_first = site.surface + site.width;
        if after_first > site.end {
            self.add_unknown::<BEST>(dictionary, unknown, site.char_info, &site.span(after_first));
            if self.node_count::<BEST>() > first {
                return;
            }
        }
        let group_end = if site.char_info.group() {
            Some(self.add_grouped_unknown::<BEST>(dictionary, unknown, sentence, site, after_first))
        } else {
            None
        };
        let last = self.add_stepped_unknown::<BEST>(
            dictionary,
            unknown,
            sentence,
            site,
            (after_first, group_end),
        );
        if self.node_count::<BEST>() == first {
            self.add_unknown::<BEST>(dictionary, unknown, site.char_info, &site.span(last));
        }
    }

    #[inline(always)]
    fn add_grouped_unknown<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        unknown: &PackedDictionary,
        sentence: &[u8],
        site: &UnknownSite,
        after_first: usize,
    ) -> usize {
        let (grouped_end, group_scan) = if site.whole {
            self.sentence_chars
                .seek_other_type(after_first, site.char_info)
        } else {
            dictionary
                .chars
                .seek_other_type(sentence, after_first, site.end, site.char_info)
        };
        if group_scan.count <= dictionary.max_grouping_size.get() {
            self.add_unknown::<BEST>(dictionary, unknown, site.char_info, &site.span(grouped_end));
        }
        grouped_end
    }

    #[inline(always)]
    fn add_stepped_unknown<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        unknown: &PackedDictionary,
        sentence: &[u8],
        site: &UnknownSite,
        (mut after_char, group_end): (usize, Option<usize>),
    ) -> usize {
        let char_info = site.char_info;
        for _ in 1..=char_info.length() {
            if after_char > site.end {
                break;
            }
            if Some(after_char) == group_end {
                continue;
            }
            self.add_unknown::<BEST>(dictionary, unknown, char_info, &site.span(after_char));
            if after_char == site.end {
                break;
            }
            let (next_info, width) = if site.whole {
                self.sentence_chars.char_info(after_char)
            } else {
                dictionary.chars.char_info(sentence, after_char, site.end)
            };
            if !char_info.is_kind_of(next_info) {
                break;
            }
            after_char += width;
        }
        after_char
    }

    #[inline]
    fn push_packed_hits<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        words: &PackedDictionary,
        view: &PackedView<'_>,
        sentence: &[u8],
        found: &HitSite,
    ) {
        for hit_index in 0..self.hits.len() {
            let hit = self.hits[hit_index];
            let (hit_end, end) = hit_ends(dictionary, sentence, found, hit);
            let source = if end == hit_end {
                NodeSource::System
            } else {
                NodeSource::SystemExtended
            };
            for token in view.tokens(hit.tokens()) {
                if !BEST
                    && found.partial
                    && !self.constraints.admits(found.begin, end, |out| {
                        let key = &sentence[found.surface..hit_end];
                        words.write_feature(token.feature, str::from_utf8(key).unwrap_or(""), out);
                    })
                {
                    continue;
                }
                let candidate =
                    Candidate::from_token(&token, found.surface, end, source, found.char_type);
                self.push_candidate::<BEST>(found.begin, candidate);
            }
        }
    }

    #[inline]
    fn push_system_hits<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        words: &SystemDictionary,
        source: NodeSource,
        sentence: &[u8],
        found: &HitSite,
    ) {
        for hit_index in 0..self.hits.len() {
            let hit = self.hits[hit_index];
            let (_, end) = hit_ends(dictionary, sentence, found, hit);
            for token_index in hit.tokens() {
                let token = words.token(token_index);
                if !BEST
                    && found.partial
                    && !self.constraints.admits(found.begin, end, |out| {
                        out.push_str(words.feature(token.feature));
                    })
                {
                    continue;
                }
                let candidate =
                    Candidate::from_token(&token, found.surface, end, source, found.char_type);
                self.push_candidate::<BEST>(found.begin, candidate);
            }
        }
    }

    pub(super) fn add_constrained_unknown(
        &mut self,
        dictionary: &Dictionary,
        unknown: &PackedDictionary,
        sentence: &[u8],
        begin: usize,
        surface: usize,
        end: usize,
    ) -> Result<(), Error> {
        let first = self.nodes.len();
        let mut cursor = surface;
        let char_limit = if surface == end { sentence.len() } else { end };
        let char_info = loop {
            let (info, width) = dictionary.chars.char_info(sentence, cursor, char_limit);
            cursor += width;
            if cursor > end || !self.constraints.is_inside_token(cursor) {
                break info;
            }
        };
        let span = Span::new(begin, surface, cursor);
        self.add_unknown::<false>(dictionary, unknown, char_info, &span);
        if self.nodes.len() > first {
            return Ok(());
        }
        let Some(feature) = self.constraints.feature_at(begin) else {
            return Err(Error::UnsatisfiableConstraint);
        };
        self.push_node(RawNode {
            surface_start: surface as u32,
            length: (cursor - surface) as u32,
            length_with_space: (cursor - begin) as u32,
            left_id: 0,
            right_id: 0,
            word_cost: 0,
            pos_id: 0,
            source: NodeSource::Constrained,
            char_type: char_info.default_type() as u8,
            feature,
            cost: 0,
            prev: None,
            links: LinkSpan::default(),
        });
        Ok(())
    }

    #[inline(always)]
    pub(super) fn add_unknown<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        unknown: &PackedDictionary,
        char_info: CharInfo,
        span: &Span,
    ) {
        let tokens = &dictionary.unknown_tokens[char_info.default_type()];
        let source = if dictionary.unknown_feature.is_some() {
            NodeSource::UnknownFixed
        } else {
            NodeSource::Unknown
        };
        for &token in tokens {
            if !BEST
                && self.constraints.is_active()
                && !self.constraints.admits(span.begin, span.end, |out| {
                    match &dictionary.unknown_feature {
                        Some(feature) => out.push_str(feature),
                        None => unknown.write_feature(token.feature, "", out),
                    }
                })
            {
                continue;
            }
            let candidate = Candidate::from_token(
                &token,
                span.surface,
                span.end,
                source,
                char_info.default_type() as u8,
            );
            self.push_candidate::<BEST>(span.begin, candidate);
        }
    }
}
