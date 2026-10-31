use super::{
    Candidate, CharInfo, Dictionary, Error, HitSite, Lattice, LinkSpan, NodeSource,
    PackedDictionary, PackedView, Range, RawNode, Span, SystemDictionary, UserIndex,
};

impl Lattice {
    #[inline(always)]
    pub(super) fn lookup<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        system: &PackedDictionary,
        view: &PackedView<'_>,
        unknown: &PackedDictionary,
        sentence: &[u8],
        begin: usize,
    ) -> Result<Range<usize>, Error> {
        let chars = &dictionary.chars;
        let sentence_len = sentence.len();
        let first = self.node_count::<BEST>();
        let mut end = sentence_len;
        let partial = !BEST && self.constraints.is_active();
        if partial
            && let Some(boundary) = (begin + 1..sentence_len)
                .find(|&position| self.constraints.is_token_boundary(position))
        {
            end = boundary;
        }
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
        let key_end = if end == surface { sentence_len } else { end };
        let key = &sentence[surface..key_end];
        let found = HitSite {
            begin,
            surface,
            partial,
            char_type: char_info.default_type() as u8,
            limit: end,
        };
        if whole {
            view.common_prefix_search_sentence(&self.sentence_chars, surface, &mut self.hits);
        } else {
            view.common_prefix_search(key, &mut self.hits);
        }
        self.push_packed_hits::<BEST>(
            dictionary,
            system,
            view,
            NodeSource::System,
            sentence,
            &found,
        );
        for (index, user) in dictionary.users.iter().enumerate() {
            user.common_prefix_search(key, &mut self.hits);
            self.push_system_hits::<BEST>(
                dictionary,
                user,
                NodeSource::User(UserIndex::from_position(index)),
                sentence,
                &found,
            );
        }
        if self.node_count::<BEST>() > first && !char_info.invoke() {
            return Ok(first..self.node_count::<BEST>());
        }
        let mut after_char = surface + scan.width;
        let mut group_end: Option<usize> = None;
        if after_char > end {
            let span = Span {
                begin,
                surface,
                end: after_char,
            };
            self.add_unknown::<BEST>(dictionary, unknown, char_info, &span);
            if self.node_count::<BEST>() > first {
                return Ok(first..self.node_count::<BEST>());
            }
        }
        if char_info.group() {
            let saved = after_char;
            let (grouped_end, group_scan) = if whole {
                self.sentence_chars.seek_other_type(after_char, char_info)
            } else {
                chars.seek_other_type(sentence, after_char, end, char_info)
            };
            after_char = grouped_end;
            if group_scan.count <= dictionary.max_grouping_size.get() {
                let span = Span {
                    begin,
                    surface,
                    end: after_char,
                };
                self.add_unknown::<BEST>(dictionary, unknown, char_info, &span);
            }
            group_end = Some(after_char);
            after_char = saved;
        }
        for _ in 1..=char_info.length() {
            if after_char > end {
                break;
            }
            if Some(after_char) == group_end {
                continue;
            }
            let span = Span {
                begin,
                surface,
                end: after_char,
            };
            self.add_unknown::<BEST>(dictionary, unknown, char_info, &span);
            let (next_info, width) = if whole {
                self.sentence_chars.char_info(after_char)
            } else {
                chars.char_info(sentence, after_char, end)
            };
            if !char_info.is_kind_of(next_info) {
                break;
            }
            after_char += width;
        }
        if self.node_count::<BEST>() == first {
            let span = Span {
                begin,
                surface,
                end: after_char,
            };
            self.add_unknown::<BEST>(dictionary, unknown, char_info, &span);
        }
        if partial && self.node_count::<BEST>() == first {
            self.add_constrained_unknown(dictionary, unknown, sentence, begin, surface, end)?;
        }
        Ok(first..self.node_count::<BEST>())
    }

    #[inline]
    pub(super) fn push_packed_hits<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        words: &PackedDictionary,
        view: &PackedView<'_>,
        source: NodeSource,
        sentence: &[u8],
        found: &HitSite,
    ) {
        for hit_index in 0..self.hits.len() {
            let hit = self.hits[hit_index];
            let end =
                dictionary
                    .chars
                    .extended_end(sentence, found.surface + hit.length(), found.limit);
            let source = if end == found.surface + hit.length() {
                source
            } else {
                NodeSource::SystemExtended
            };
            for token in view.tokens(hit.tokens()) {
                if !BEST
                    && found.partial
                    && !self.constraints.admits(found.begin, end, |out| {
                        let key = &sentence[found.surface..found.surface + hit.length()];
                        words.write_feature(token.feature, str::from_utf8(key).unwrap_or(""), out);
                    })
                {
                    continue;
                }
                let candidate = Candidate {
                    surface: found.surface as u32,
                    end: end as u32,
                    feature: token.feature,
                    left_id: token.left_id,
                    right_id: token.right_id,
                    pos_id: token.pos_id,
                    word_cost: token.word_cost,
                    source,
                    char_type: found.char_type,
                };
                self.push_candidate::<BEST>(found.begin, candidate);
            }
        }
    }

    #[inline]
    pub(super) fn push_system_hits<const BEST: bool>(
        &mut self,
        dictionary: &Dictionary,
        words: &SystemDictionary,
        source: NodeSource,
        sentence: &[u8],
        found: &HitSite,
    ) {
        for hit_index in 0..self.hits.len() {
            let hit = self.hits[hit_index];
            let end =
                dictionary
                    .chars
                    .extended_end(sentence, found.surface + hit.length(), found.limit);
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
                let candidate = Candidate {
                    surface: found.surface as u32,
                    end: end as u32,
                    feature: token.feature,
                    left_id: token.left_id,
                    right_id: token.right_id,
                    pos_id: token.pos_id,
                    word_cost: token.word_cost,
                    source,
                    char_type: found.char_type,
                };
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
        let char_info = loop {
            let (info, width) = dictionary.chars.char_info(sentence, cursor, end);
            cursor += width;
            if cursor > end || !self.constraints.is_inside_token(cursor) {
                break info;
            }
        };
        let span = Span {
            begin,
            surface,
            end: cursor,
        };
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
            let candidate = Candidate {
                surface: span.surface as u32,
                end: span.end as u32,
                feature: token.feature,
                left_id: token.left_id,
                right_id: token.right_id,
                pos_id: token.pos_id,
                word_cost: token.word_cost,
                source,
                char_type: char_info.default_type() as u8,
            };
            self.push_candidate::<BEST>(span.begin, candidate);
        }
    }
}
