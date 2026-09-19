//! Vendored fuzzysort v3.1.0 (`fuzzysort.js`, pinned in the TS lockfile).
//!
//! Faithful port of the `go` search path (its "no keys" branch) including
//! the min-heap tie behavior, so result ordering matches the TS reference
//! for ASCII targets. Strings are processed as UTF-16 code units like the
//! JS original.

// ---------------------------------------------------------------------------
// accents (prepareLowerInfo's remove_accents)
// ---------------------------------------------------------------------------

/// The NFD decomposition of one Latin-1 Supplement / Latin Extended-A code
/// point — the `\p{Script=Latin}` runs `remove_accents` decomposes in
/// practice (`fuzzysort.js:591`). Non-Latin scripts pass through,
/// matching the regex.
fn latin_nfd(cp: u32) -> Option<&'static str> {
    Some(match cp {
        0xC0 => "A\u{300}",
        0xC1 => "A\u{301}",
        0xC2 => "A\u{302}",
        0xC3 => "A\u{303}",
        0xC4 => "A\u{308}",
        0xC5 => "A\u{30a}",
        0xC7 => "C\u{327}",
        0xC8 => "E\u{300}",
        0xC9 => "E\u{301}",
        0xCA => "E\u{302}",
        0xCB => "E\u{308}",
        0xCC => "I\u{300}",
        0xCD => "I\u{301}",
        0xCE => "I\u{302}",
        0xCF => "I\u{308}",
        0xD1 => "N\u{303}",
        0xD2 => "O\u{300}",
        0xD3 => "O\u{301}",
        0xD4 => "O\u{302}",
        0xD5 => "O\u{303}",
        0xD6 => "O\u{308}",
        0xD9 => "U\u{300}",
        0xDA => "U\u{301}",
        0xDB => "U\u{302}",
        0xDC => "U\u{308}",
        0xDD => "Y\u{301}",
        0xE0 => "a\u{300}",
        0xE1 => "a\u{301}",
        0xE2 => "a\u{302}",
        0xE3 => "a\u{303}",
        0xE4 => "a\u{308}",
        0xE5 => "a\u{30a}",
        0xE7 => "c\u{327}",
        0xE8 => "e\u{300}",
        0xE9 => "e\u{301}",
        0xEA => "e\u{302}",
        0xEB => "e\u{308}",
        0xEC => "i\u{300}",
        0xED => "i\u{301}",
        0xEE => "i\u{302}",
        0xEF => "i\u{308}",
        0xF1 => "n\u{303}",
        0xF2 => "o\u{300}",
        0xF3 => "o\u{301}",
        0xF4 => "o\u{302}",
        0xF5 => "o\u{303}",
        0xF6 => "o\u{308}",
        0xF9 => "u\u{300}",
        0xFA => "u\u{301}",
        0xFB => "u\u{302}",
        0xFC => "u\u{308}",
        0xFD => "y\u{301}",
        0xFF => "y\u{308}",
        0x100 => "A\u{304}",
        0x101 => "a\u{304}",
        0x102 => "A\u{306}",
        0x103 => "a\u{306}",
        0x104 => "A\u{328}",
        0x105 => "a\u{328}",
        0x106 => "C\u{301}",
        0x107 => "c\u{301}",
        0x108 => "C\u{302}",
        0x109 => "c\u{302}",
        0x10A => "C\u{307}",
        0x10B => "c\u{307}",
        0x10C => "C\u{30c}",
        0x10D => "c\u{30c}",
        0x10E => "D\u{30c}",
        0x10F => "d\u{30c}",
        0x112 => "E\u{304}",
        0x113 => "e\u{304}",
        0x114 => "E\u{302}",
        0x115 => "e\u{302}",
        0x116 => "E\u{307}",
        0x117 => "e\u{307}",
        0x118 => "E\u{328}",
        0x119 => "e\u{328}",
        0x11A => "E\u{30c}",
        0x11B => "e\u{30c}",
        0x11C => "G\u{302}",
        0x11D => "g\u{302}",
        0x11E => "G\u{306}",
        0x11F => "g\u{306}",
        0x120 => "G\u{307}",
        0x121 => "g\u{307}",
        0x122 => "G\u{327}",
        0x123 => "g\u{327}",
        0x124 => "H\u{302}",
        0x125 => "h\u{302}",
        0x128 => "I\u{303}",
        0x129 => "i\u{303}",
        0x12A => "I\u{304}",
        0x12B => "i\u{304}",
        0x12C => "I\u{306}",
        0x12D => "i\u{306}",
        0x12E => "I\u{328}",
        0x12F => "i\u{328}",
        0x130 => "I\u{307}",
        0x134 => "J\u{302}",
        0x135 => "j\u{302}",
        0x137 => "K\u{327}",
        0x138 => "k\u{327}",
        0x139 => "L\u{301}",
        0x13A => "l\u{301}",
        0x13B => "L\u{327}",
        0x13C => "l\u{327}",
        0x13D => "L\u{30c}",
        0x13E => "l\u{30c}",
        0x142 => "N\u{301}",
        0x143 => "n\u{301}",
        0x144 => "N\u{327}",
        0x145 => "n\u{327}",
        0x146 => "N\u{30c}",
        0x147 => "n\u{30c}",
        0x14C => "O\u{304}",
        0x14D => "o\u{304}",
        0x14E => "O\u{306}",
        0x14F => "o\u{306}",
        0x150 => "O\u{30b}",
        0x151 => "o\u{30b}",
        0x154 => "R\u{301}",
        0x155 => "r\u{301}",
        0x156 => "R\u{327}",
        0x157 => "r\u{327}",
        0x158 => "R\u{30c}",
        0x159 => "r\u{30c}",
        0x15A => "S\u{301}",
        0x15B => "s\u{301}",
        0x15C => "S\u{302}",
        0x15D => "s\u{302}",
        0x15E => "S\u{327}",
        0x15F => "s\u{327}",
        0x160 => "S\u{30c}",
        0x161 => "s\u{30c}",
        0x162 => "T\u{327}",
        0x163 => "t\u{327}",
        0x164 => "T\u{30c}",
        0x165 => "t\u{30c}",
        0x168 => "U\u{303}",
        0x169 => "u\u{303}",
        0x16A => "U\u{304}",
        0x16B => "u\u{304}",
        0x16C => "U\u{306}",
        0x16D => "u\u{306}",
        0x16E => "U\u{30a}",
        0x16F => "u\u{30a}",
        0x170 => "U\u{30b}",
        0x171 => "u\u{30b}",
        0x176 => "Y\u{302}",
        0x177 => "y\u{302}",
        0x178 => "Y\u{308}",
        0x179 => "Z\u{301}",
        0x17A => "z\u{301}",
        0x17B => "Z\u{307}",
        0x17C => "z\u{307}",
        0x17D => "Z\u{30c}",
        0x17E => "z\u{30c}",
        _ => return None,
    })
}

/// `remove_accents` (`fuzzysort.js:591`) — decompose Latin runs (NFD), then
/// strip combining marks U+0300-U+036F.
fn remove_accents(input: &str) -> String {
    let mut decomposed = String::with_capacity(input.len());
    for ch in input.chars() {
        match latin_nfd(ch as u32) {
            Some(base) => decomposed.push_str(base),
            None => decomposed.push(ch),
        }
    }
    decomposed
        .chars()
        .filter(|ch| !('\u{300}'..='\u{36f}').contains(ch))
        .collect()
}

// ---------------------------------------------------------------------------
// prepared search/target
// ---------------------------------------------------------------------------

/// `prepareLowerInfo` (`fuzzysort.js:593-618`).
struct LowerInfo {
    lower_codes: Vec<u16>,
    lower: String,
    bitflags: u32,
    contains_space: bool,
}

fn prepare_lower_info(input: &str) -> LowerInfo {
    let cleaned = remove_accents(input);
    let lower = cleaned.to_lowercase();
    let mut lower_codes = Vec::with_capacity(lower.len());
    let mut bitflags = 0u32;
    let mut contains_space = false;
    for code in lower.encode_utf16() {
        if code == 32 {
            contains_space = true;
            continue;
        }
        lower_codes.push(code);
        let bit = if (97..=122).contains(&code) {
            (code - 97) as u32
        } else if (48..=57).contains(&code) {
            26
        } else if code <= 127 {
            30
        } else {
            31
        };
        bitflags |= 1 << bit;
    }
    LowerInfo {
        lower_codes,
        lower,
        bitflags,
        contains_space,
    }
}

/// A prepared target (`fuzzysort.js:224-229`, `prepare`).
struct Prepared {
    lower: String,
    lower_codes: Vec<u16>,
    bitflags: u32,
    /// Computed over the accent-removed, *non-lowercased* target like the
    /// JS original (`prepareNextBeginningIndexes` reads `prepared.target`).
    next_beginning_indexes: Option<Vec<usize>>,
    /// UTF-16 units of `remove_accents(target)` — the lazily-built table's
    /// source.
    cleaned_units: Vec<u16>,
}

fn prepare(target: &str) -> Prepared {
    let info = prepare_lower_info(target);
    let cleaned_units: Vec<u16> = remove_accents(target).encode_utf16().collect();
    Prepared {
        lower: info.lower,
        lower_codes: info.lower_codes,
        bitflags: info.bitflags,
        next_beginning_indexes: None,
        cleaned_units,
    }
}

impl Prepared {
    fn ensure_beginning_indexes(&mut self) {
        if self.next_beginning_indexes.is_none() {
            let units = std::mem::take(&mut self.cleaned_units);
            self.next_beginning_indexes = Some(next_beginning_indexes(&units));
        }
    }
}

/// One search token of a space-separated search (`prepareSearch`'s
/// `spaceSearches` entries, `fuzzysort.js:285-294`).
struct SpaceSearch {
    lower_codes: Vec<u16>,
    lower: String,
}

/// `prepareSearch` (`fuzzysort.js:279-297`).
struct PreparedSearch {
    lower_codes: Vec<u16>,
    lower: String,
    contains_space: bool,
    bitflags: u32,
    space_searches: Vec<SpaceSearch>,
}

fn prepare_search(search: &str) -> PreparedSearch {
    let search = search.trim();
    let info = prepare_lower_info(search);
    let mut space_searches = Vec::new();
    if info.contains_space {
        let mut seen = std::collections::HashSet::new();
        for part in search.split_whitespace() {
            if !seen.insert(part) {
                continue;
            }
            let part_info = prepare_lower_info(part);
            space_searches.push(SpaceSearch {
                lower_codes: part_info.lower_codes,
                lower: part_info.lower,
            });
        }
    }
    PreparedSearch {
        lower_codes: info.lower_codes,
        lower: info.lower,
        contains_space: info.contains_space,
        bitflags: info.bitflags,
        space_searches,
    }
}

// ---------------------------------------------------------------------------
// beginning indexes
// ---------------------------------------------------------------------------

/// `prepareBeginningIndexes` (`fuzzysort.js:619-634`).
fn prepare_beginning_indexes(target: &[u16]) -> Vec<usize> {
    let mut beginning_indexes = Vec::new();
    let mut was_upper = false;
    let mut was_alphanum = false;
    for (index, &code) in target.iter().enumerate() {
        let is_upper = (65..=90).contains(&code);
        let is_alphanum = is_upper || (97..=122).contains(&code) || (48..=57).contains(&code);
        let is_beginning = is_upper && !was_upper || !was_alphanum || !is_alphanum;
        was_upper = is_upper;
        was_alphanum = is_alphanum;
        if is_beginning {
            beginning_indexes.push(index);
        }
    }
    beginning_indexes
}

/// `prepareNextBeginningIndexes` (`fuzzysort.js:635-651`).
fn next_beginning_indexes(target: &[u16]) -> Vec<usize> {
    let beginning_indexes = prepare_beginning_indexes(target);
    let target_len = target.len();
    let mut out = Vec::with_capacity(target_len);
    let mut current = beginning_indexes.first().copied();
    let mut beginning_i = 0usize;
    for index in 0..target_len {
        if current.is_some_and(|last| last > index) {
            out.push(current.unwrap());
        } else {
            beginning_i += 1;
            current = beginning_indexes.get(beginning_i).copied();
            out.push(current.unwrap_or(target_len));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// algorithm
// ---------------------------------------------------------------------------

/// A scored match — the JS `Result` fields the consumers read (`.target`)
/// plus the internal `_score` and `_indexes` (`algorithmSpaces` uses both).
struct Match {
    score: f64,
    indexes: Vec<usize>,
}

/// `String.prototype.indexOf` with a from-index over UTF-16 code units.
fn index_of_utf16(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let start = from.min(haystack.len());
    if start + needle.len() > haystack.len() {
        return None;
    }
    (start..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// `algorithm` (`fuzzysort.js:361-499`). Returns `None` where the JS
/// original returns `NULL`.
fn algorithm(
    search_lower_codes: &[u16],
    search_lower: &str,
    contains_space: bool,
    prepared_search: &PreparedSearch,
    target: &mut Prepared,
    allow_spaces: bool,
    allow_partial_match: bool,
) -> Option<Match> {
    if !allow_spaces && contains_space {
        return algorithm_spaces(prepared_search, target, allow_partial_match);
    }
    let search_len = search_lower_codes.len();
    let target_len = target.lower_codes.len();
    if search_len == 0 || target_len == 0 {
        return None;
    }

    // Simple sequential-match pass.
    let mut matches_simple: Vec<usize> = Vec::with_capacity(search_len);
    let mut search_i = 0usize;
    let mut search_lower_code = search_lower_codes[0];
    let mut target_i = 0usize;
    loop {
        if search_lower_code == target.lower_codes[target_i] {
            matches_simple.push(target_i);
            search_i += 1;
            if search_i == search_len {
                break;
            }
            search_lower_code = search_lower_codes[search_i];
        }
        target_i += 1;
        if target_i >= target_len {
            return None;
        }
    }

    // Strict pass over the beginning indexes.
    target.ensure_beginning_indexes();
    let next = target
        .next_beginning_indexes
        .as_deref()
        .expect("initialized");
    let next_at = |i: usize| -> usize { next.get(i).copied().unwrap_or(target_len) };
    let mut search_i = 0usize;
    let mut success_strict = false;
    let mut matches_strict: Vec<usize> = Vec::with_capacity(search_len);
    let mut target_i = if matches_simple[0] == 0 {
        0
    } else {
        next_at(matches_simple[0] - 1)
    };
    let mut backtrack_count = 0;
    if target_i != target_len {
        loop {
            if target_i >= target_len {
                if search_i == 0 {
                    break;
                }
                backtrack_count += 1;
                if backtrack_count > 200 {
                    break;
                }
                search_i -= 1;
                let last_match = matches_strict.pop().unwrap_or(0);
                target_i = next_at(last_match);
            } else if search_lower_codes[search_i] == target.lower_codes[target_i] {
                matches_strict.push(target_i);
                search_i += 1;
                if search_i == search_len {
                    success_strict = true;
                    break;
                }
                target_i += 1;
            } else {
                target_i = next_at(target_i);
            }
        }
    }

    // Substring match?
    let target_lower: Vec<u16> = target.lower.encode_utf16().collect();
    let search_lower_codes_utf16: Vec<u16> = search_lower.encode_utf16().collect();
    let mut substring_index: i64 = if search_len <= 1 {
        -1
    } else {
        index_of_utf16(&target_lower, &search_lower_codes_utf16, matches_simple[0])
            .map(|i| i as i64)
            .unwrap_or(-1)
    };
    let is_substring = substring_index != -1;
    let mut is_substring_beginning = if !is_substring {
        false
    } else {
        substring_index == 0 || next_at(substring_index as usize - 1) == substring_index as usize
    };
    if is_substring && !is_substring_beginning {
        let mut i = 0usize;
        while i < next.len() {
            if (i as i64) <= substring_index {
                i = next_at(i);
                continue;
            }
            if i + search_len <= target_len {
                let mut s = 0usize;
                while s < search_len && search_lower_codes[s] == target.lower_codes[i + s] {
                    s += 1;
                }
                if s == search_len {
                    substring_index = i as i64;
                    is_substring_beginning = true;
                    break;
                }
            }
            i = next_at(i);
        }
    }

    let calculate_score = |matches: &[usize]| -> f64 {
        let mut score = 0.0f64;
        let mut extra_match_group_count = 0.0f64;
        for i in 1..search_len {
            if matches[i] - matches[i - 1] != 1 {
                score -= matches[i] as f64;
                extra_match_group_count += 1.0;
            }
        }
        let unmatched_distance = matches[search_len - 1] - matches[0] - (search_len - 1);
        score -= (12.0 + unmatched_distance as f64) * extra_match_group_count;
        if matches[0] != 0 {
            score -= matches[0] as f64 * matches[0] as f64 * 0.2;
        }
        if !success_strict {
            score *= 1000.0;
        } else {
            let mut unique_beginning_indexes = 1usize;
            let mut i = next_at(0);
            while i < target_len {
                unique_beginning_indexes += 1;
                i = next_at(i);
            }
            if unique_beginning_indexes > 24 {
                score *= ((unique_beginning_indexes - 24) * 10) as f64;
            }
        }
        score -= (target_len - search_len) as f64 / 2.0;
        if is_substring {
            score /= 1.0 + (search_len * search_len) as f64;
        }
        if is_substring_beginning {
            score /= 1.0 + (search_len * search_len) as f64;
        }
        score -= (target_len - search_len) as f64 / 2.0;
        score
    };

    let matches_best: Vec<usize> = if !success_strict || is_substring_beginning {
        if is_substring {
            matches_simple.iter_mut().enumerate().for_each(|(i, slot)| {
                *slot = (substring_index + i as i64) as usize;
            });
        }
        matches_simple
    } else {
        matches_strict
    };
    let score = calculate_score(&matches_best);
    Some(Match {
        score,
        indexes: matches_best,
    })
}

/// `algorithmSpaces` (`fuzzysort.js:500-588`).
fn algorithm_spaces(
    prepared_search: &PreparedSearch,
    target: &mut Prepared,
    allow_partial_match: bool,
) -> Option<Match> {
    let searches_len = prepared_search.space_searches.len();
    let mut score = 0.0f64;
    let mut first_seen_index_last_search = 0usize;
    let mut has_at_least_one_match = false;
    let mut changes: Vec<(usize, usize)> = Vec::new();
    let mut result: Option<Match> = None;

    for (index, search) in prepared_search.space_searches.iter().enumerate() {
        let match_ = algorithm(
            &search.lower_codes,
            &search.lower,
            false,
            prepared_search,
            target,
            false,
            false,
        );
        let match_ = match match_ {
            Some(match_) => {
                if allow_partial_match {
                    has_at_least_one_match = true;
                }
                match_
            }
            None => {
                if allow_partial_match {
                    continue;
                }
                reset_beginning_indexes(target, &changes);
                return None;
            }
        };

        // Mutate `_nextBeginningIndexes` so the next search sees the end
        // of this match's consecutive substring as a beginning index.
        let is_the_last_search = index == searches_len - 1;
        if !is_the_last_search {
            let indexes = &match_.indexes;
            let mut consecutive = true;
            for i in 0..indexes.len().saturating_sub(1) {
                if indexes[i + 1] - indexes[i] != 1 {
                    consecutive = false;
                    break;
                }
            }
            if consecutive && !indexes.is_empty() {
                let new_beginning_index = indexes[indexes.len() - 1] + 1;
                let to_replace = target
                    .next_beginning_indexes
                    .as_ref()
                    .expect("initialized by algorithm")
                    .get(new_beginning_index - 1)
                    .copied();
                if let Some(to_replace) = to_replace {
                    let mut i = new_beginning_index - 1;
                    loop {
                        let break_needed = {
                            let table = target
                                .next_beginning_indexes
                                .as_mut()
                                .expect("initialized by algorithm");
                            if i >= table.len() || table[i] != to_replace {
                                true
                            } else {
                                table[i] = new_beginning_index;
                                changes.push((i, to_replace));
                                false
                            }
                        };
                        if break_needed || i == 0 {
                            break;
                        }
                        i -= 1;
                    }
                }
            }
        }

        score += match_.score / searches_len as f64;
        let first_index = match_.indexes.first().copied().unwrap_or(0);
        if first_index < first_seen_index_last_search {
            score -= ((first_seen_index_last_search - first_index) * 2) as f64;
        }
        first_seen_index_last_search = first_index;
        result = Some(match_);
    }

    if allow_partial_match && !has_at_least_one_match {
        return None;
    }
    reset_beginning_indexes(target, &changes);

    // A search with spaces that's an exact substring scores well.
    let allow_spaces_result = algorithm(
        &prepared_search.lower_codes,
        &prepared_search.lower,
        true,
        prepared_search,
        target,
        true,
        prepared_search.contains_space,
    );
    if let Some(allow_spaces) = allow_spaces_result {
        if allow_spaces.score > score {
            return Some(allow_spaces);
        }
    }
    result.map(|mut result| {
        result.score = score;
        result
    })
}

/// `resetNextBeginningIndexes` (`fuzzysort.js:511-513`) — restore in reverse
/// order.
fn reset_beginning_indexes(target: &mut Prepared, changes: &[(usize, usize)]) {
    for (i, previous) in changes.iter().rev() {
        if let Some(slot) = target.next_beginning_indexes.as_mut() {
            slot[*i] = *previous;
        }
    }
}

// ---------------------------------------------------------------------------
// go
// ---------------------------------------------------------------------------

/// `go` options — `limit: None` is JS `undefined` (the `|| INFINITY`
/// normalization applies inside).
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub limit: Option<usize>,
    pub threshold: Option<f64>,
}

/// The FastPriorityQueue port (`fuzzysort.js:685`) — a min-heap by score
/// with the exact sift semantics of the vendored JS implementation.
struct MinHeap {
    items: Vec<(f64, usize)>,
}

impl MinHeap {
    fn percolate(&mut self) {
        let size = self.items.len();
        if size == 0 {
            return;
        }
        let mut i = 0usize;
        let value = self.items[0];
        let mut child = 1usize;
        while child < size {
            let mut chosen = child;
            if child + 1 < size && self.items[child + 1].0 < self.items[child].0 {
                chosen = child + 1;
            }
            i = chosen;
            let parent = (i - 1) >> 1;
            self.items[parent] = self.items[i];
            child = 1 + (i << 1);
        }
        while i > 0 {
            let parent = (i - 1) >> 1;
            if value.0 < self.items[parent].0 {
                self.items[i] = self.items[parent];
            } else {
                break;
            }
            i = parent;
        }
        self.items[i] = value;
    }

    fn add(&mut self, score: f64, target_index: usize) {
        let mut i = self.items.len();
        self.items.push((score, target_index));
        while i > 0 {
            let parent = (i - 1) >> 1;
            if score < self.items[parent].0 {
                self.items[i] = self.items[parent];
            } else {
                break;
            }
            i = parent;
        }
        self.items[i] = (score, target_index);
    }

    fn poll(&mut self) -> (f64, usize) {
        let root = self.items[0];
        let last = self.items.pop().expect("poll on non-empty heap");
        if !self.items.is_empty() {
            self.items[0] = last;
            self.percolate();
        }
        root
    }

    fn peek_score(&self) -> f64 {
        self.items[0].0
    }

    fn replace_top(&mut self, score: f64, target_index: usize) {
        self.items[0] = (score, target_index);
        self.percolate();
    }
}

/// `fuzzysort.go(search, targets, options)` — the "no keys" branch
/// (`fuzzysort.js:23-170`). Returns indexes into `targets`, best first.
pub fn go(search: &str, targets: &[String], options: &Options) -> Vec<usize> {
    if search.is_empty() {
        return Vec::new();
    }
    let prepared_search = prepare_search(search);
    let search_bitflags = prepared_search.bitflags;
    // `options?.threshold || 0` then denormalize; `-10000` denormalizes to
    // NaN in JS, which disables the filter (NaN comparisons are false).
    let threshold = denormalize_score(options.threshold.unwrap_or(0.0));
    let limit = options.limit.filter(|limit| *limit > 0);

    let mut heap = MinHeap { items: Vec::new() };
    for (target_index, target) in targets.iter().enumerate() {
        if target.is_empty() {
            continue;
        }
        let mut prepared = prepare(target);
        if (search_bitflags & prepared.bitflags) != search_bitflags {
            continue;
        }
        let Some(result) = algorithm(
            &prepared_search.lower_codes,
            &prepared_search.lower,
            prepared_search.contains_space,
            &prepared_search,
            &mut prepared,
            false,
            false,
        ) else {
            continue;
        };
        if result.score < threshold {
            continue;
        }
        match limit {
            Some(limit) if heap.items.len() < limit => heap.add(result.score, target_index),
            Some(_) => {
                if result.score > heap.peek_score() {
                    heap.replace_top(result.score, target_index);
                }
            }
            None => heap.add(result.score, target_index),
        }
    }
    let mut out: Vec<usize> = Vec::with_capacity(heap.items.len());
    while !heap.items.is_empty() {
        let (_, target_index) = heap.poll();
        out.push(target_index);
    }
    out.reverse();
    out
}

/// `denormalizeScore` (`fuzzysort.js:272-275`).
fn denormalize_score(normalized_score: f64) -> f64 {
    if normalized_score == 0.0 {
        return f64::NEG_INFINITY;
    }
    if normalized_score > 1.0 {
        return normalized_score;
    }
    1.0 - ((normalized_score.ln() / -2.0 + 1.0).powf(1.0 / 0.04307))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(query: &str, targets: &[&str], limit: Option<usize>) -> Vec<String> {
        let items: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
        go(
            query,
            &items,
            &Options {
                limit,
                threshold: None,
            },
        )
        .into_iter()
        .map(|i| items[i].clone())
        .collect()
    }

    #[test]
    fn substring_beats_scattered() {
        let results = run(
            "repo",
            &["my-repo-x", "the/r(e)p(o) files", "documents"],
            None,
        );
        assert_eq!(results.first().map(String::as_str), Some("my-repo-x"));
    }

    #[test]
    fn limit_keeps_best() {
        let results = run("test", &["a-test", "b-test", "test-c", "nope"], Some(2));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], "test-c");
        assert_eq!(results[1], "b-test");
    }

    #[test]
    fn no_match_returns_empty() {
        assert!(run("zzz", &["abc", "def"], None).is_empty());
        assert!(run("", &["abc"], None).is_empty());
        // Bitflags filter — the query has a character the target lacks.
        assert!(run("q", &["abc"], None).is_empty());
    }

    #[test]
    fn space_query_matches_each_token() {
        let results = run("read me", &["readme.md", "writeup"], None);
        assert_eq!(results, vec!["readme.md".to_string()]);
    }

    #[test]
    fn case_and_accents_insensitive() {
        let results = run("Cafe", &["café.txt", "other"], None);
        assert_eq!(results, vec!["café.txt".to_string()]);
    }

    /// Golden orderings captured from the pinned TS fuzzysort v3.1.0.
    #[test]
    fn ts_golden_orderings() {
        assert_eq!(
            run(
                "src/main/foo.rs",
                &[
                    "src/main/foo.rs",
                    "src/foo.rs",
                    "src/main.rs",
                    "docs/readme.md",
                    "src/main/foo/bar.rs"
                ],
                None
            ),
            vec![
                "src/main/foo.rs".to_string(),
                "src/main/foo/bar.rs".to_string(),
            ]
        );
        assert_eq!(
            run(
                "cmd",
                &[
                    "src/cmd",
                    "src/common.rs",
                    ".git",
                    "cargo.toml",
                    "cmd/main.rs"
                ],
                None
            ),
            vec!["src/cmd".to_string(), "cmd/main.rs".to_string()]
        );
        assert_eq!(
            run(
                "foo",
                &["src/foo.rs", "src/foobar/baz.rs", "food.md", "src/main.rs"],
                None
            ),
            vec![
                "food.md".to_string(),
                "src/foo.rs".to_string(),
                "src/foobar/baz.rs".to_string(),
            ]
        );
        assert_eq!(
            run(
                "osver",
                &[
                    "observer_test.rs",
                    "absolute_over.rs",
                    "os_ver.rs",
                    "osver.rs"
                ],
                None
            ),
            vec![
                "osver.rs".to_string(),
                "os_ver.rs".to_string(),
                "observer_test.rs".to_string(),
            ]
        );
        assert_eq!(
            run("a", &["abc", "ABC", "a", "ba"], None),
            vec![
                "a".to_string(),
                "ABC".to_string(),
                "abc".to_string(),
                "ba".to_string(),
            ]
        );
        assert_eq!(
            run(
                "read me",
                &["readme.md", "read the me files", "i read it"],
                None
            ),
            vec!["readme.md".to_string(), "read the me files".to_string(),]
        );
    }
}
