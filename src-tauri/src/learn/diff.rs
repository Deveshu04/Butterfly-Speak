//! Picks out the misheard words a user fixed by retyping them after a paste.
//!
//! The field monitor calls [`corrections_in`] with the text it pasted
//! and the text the field holds once the user has stopped typing. The two
//! are compared word by word. Wherever the user took pasted words out and
//! typed the same number of different words in at that place, each pair of
//! words becomes a (heard, corrected) observation. Adding words, deleting
//! words, changing case and touching up punctuation teach nothing.
//!
//! The field may be a whole document with the paste somewhere inside it.
//! The engine finds the stretch of the field that best matches the paste
//! and compares only that stretch; the text around it is the user's own and
//! never produces a pair. When the paste cannot be placed with confidence,
//! nothing is reported.
//!
//! An edit that replaces most of the paste is a rewrite, and a rewrite
//! teaches nothing, not even the pairs in it that would pass on their own.
//! Every other pair must still look like the fix of a misheard word: long
//! enough on both sides, close in spelling, not already in the user's
//! dictionary, and with neither word a function word or a homophone
//! ([`NEVER_LEARNED`]).
//!
//! A pair is one observation, not a rule. [`super::candidates`] decides when
//! repeated observations become a replacement.
//!
//! PRIVACY: both texts are the user's. Nothing here logs, stores or sends
//! them.

use crate::canonical::{self, belongs_in_word};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// One observed correction: the word the transcript contained, and the word
/// the user replaced it with. Casing is preserved on both sides exactly as it
/// was observed — normalizing is the consumer's decision, and the consumer
/// cannot recover what this discards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorrectionPair {
    pub from: String,
    pub to: String,
}

/// The shortest corrected word that is learned, in Unicode scalar values of
/// the word's folded form. Set by the Devanagari tests, which learn a
/// four-scalar word (क़लम with its nukta written apart), so it can be no
/// higher. The corpus in `tests::the_fixed_word_minimum_holds_on_its_corpus`
/// shows what four rejects, every article, pronoun and short function word
/// there, and what it costs: three-scalar names and Hindi words such as "Raj"
/// and "ठीक" are not learned.
/// Same value as `MIN_FIXED_CHARS` in `src/lib/learn.ts`.
const MIN_FIXED_CHARS: usize = 4;

/// Words no edit teaches, on either side of the pair: English function
/// words, and the words people swap for grammar rather than because the
/// speech was misheard. A rule rewrites its heard word in every later
/// dictation, and which of these is right depends on the sentence ("its" or
/// "it's", "then" or "than"), so one fix must never decide it for every
/// sentence after it. Lowercase with a straight apostrophe, the form
/// [`never_learned`] looks words up in.
///
/// This list must match `NEVER_LEARNED` in `src/lib/learn.ts` word for word,
/// in the same order: the Home learner and this one must reject the same
/// pairs, or a fix refused in one place is learned in the other.
const NEVER_LEARNED: &[&str] = &[
    // Articles, determiners and quantifiers.
    "a", "an", "the", "this", "that", "these", "those", "some", "any", "each",
    "every", "all", "both", "either", "neither", "such", "much", "many", "more",
    "most", "few", "less", "other", "another", "same", "own",
    // Pronouns and possessives.
    "i", "me", "my", "mine", "myself", "you", "your", "yours", "yourself",
    "yourselves", "he", "him", "his", "himself", "she", "her", "hers", "herself",
    "it", "its", "itself", "we", "us", "our", "ours", "ourselves", "they", "them",
    "their", "theirs", "themselves", "who", "whom", "whose", "what", "which",
    "one",
    // Contractions.
    "i'm", "i'd", "i'll", "i've", "you're", "you'd", "you'll", "you've", "he's",
    "he'd", "he'll", "she's", "she'd", "she'll", "it's", "it'd", "it'll",
    "we're", "we'd", "we'll", "we've", "they're", "they'd", "they'll", "they've",
    "that's", "there's", "here's", "who's", "what's", "where's", "let's",
    "isn't", "aren't", "wasn't", "weren't", "don't", "doesn't", "didn't",
    "won't", "wouldn't", "can't", "couldn't", "shouldn't", "hasn't", "haven't",
    "hadn't",
    // Auxiliary and modal verbs.
    "am", "is", "are", "was", "were", "be", "been", "being", "do", "does", "did",
    "have", "has", "had", "will", "would", "shall", "should", "can", "could",
    "may", "might", "must",
    // Prepositions.
    "of", "off", "to", "in", "into", "on", "onto", "at", "by", "for", "from",
    "with", "within", "without", "about", "above", "after", "against", "along",
    "among", "around", "before", "behind", "below", "beside", "between",
    "beyond", "during", "except", "inside", "near", "out", "outside", "over",
    "past", "since", "through", "till", "toward", "towards", "under", "until",
    "up", "upon", "via",
    // Conjunctions.
    "and", "but", "or", "nor", "so", "yet", "if", "as", "because", "although",
    "though", "unless", "whether", "while", "than",
    // Adverbs that work like function words.
    "not", "no", "yes", "then", "there", "here", "where", "when", "why", "how",
    "now", "too", "very", "just", "also", "only",
    // Homophones, fixed for their meaning rather than their sound.
    "two", "four", "won", "hour", "know", "knew", "new", "right", "write",
    "weather", "affect", "effect", "accept", "lose", "loose", "lead", "led",
    "passed", "buy", "bye", "whole", "hole", "piece", "peace", "quiet", "quite",
    "sight", "site", "cite", "threw", "weak", "week", "wait", "weight", "break",
    "brake", "allowed", "aloud", "hear", "wear", "principal", "principle",
    "complement", "compliment", "stationary", "stationery",
    "meet", "meat", "plain", "plane", "role", "roll", "peak", "peek", "steal",
    "steel", "real", "reel", "scene", "seen", "cell", "sell", "waist", "waste",
    "desert", "dessert", "advice", "advise", "device", "devise", "later",
    "latter", "breath", "breathe", "course", "coarse", "board", "bored",
    "great", "grate", "root", "route", "rain", "reign", "stair", "stare",
    "heal", "heel", "dear", "deer", "flour", "flower", "sent", "scent", "cent",
    "patience", "patients", "presence", "presents", "lessen", "lesson",
    "ensure", "insure", "farther", "further", "personal", "personnel",
    "council", "counsel", "practice", "practise", "licence", "license", "feat",
    "feet", "heard", "herd",
];

/// The shortest heard word that is learned from, counted the same way. A
/// learned pair rewrites the heard word in every later dictation, and a
/// two-letter word is too common to hand over like that.
const MIN_HEARD_CHARS: usize = 3;

/// How far apart, as [`distance_ratio`], a heard word and its correction may
/// be. Further apart is a different word, not a fix. Chosen from the
/// labelled pairs in `tests::the_similarity_ceiling_holds_on_its_corpus`:
/// the smallest ceiling, in hundredths, that keeps the most mishearings while
/// rejecting every rewording. It keeps three edits in a seven-letter name;
/// the closest rewording it has to reject is 0.5 apart.
/// Same value as `MAX_DISTANCE` in `src/lib/learn.ts`.
const MAX_DISTANCE: f64 = 0.43;

/// Replaced words an edit may always have without being a rewrite. Chosen
/// with [`REWRITE_PERCENT`] from the labelled edits in
/// `tests::the_rewrite_threshold_holds_on_its_corpus`: a two-word paste with
/// both words fixed is a fix, and a three-word paste reworded throughout is
/// not, so two is the only allowance that fits.
/// Same value as `REWRITE_FREE_WORDS` in `src/lib/learn.ts`.
const REWRITE_FREE_WORDS: usize = 2;

/// Beyond [`REWRITE_FREE_WORDS`], the share of the pasted words, in percent,
/// an edit may replace and still be a fix. Exactly this share is a fix; one
/// word more is a rewrite. The smallest percent that accepts every fix in
/// the same corpus; three names fixed in a five-word paste set it.
/// Same value as `REWRITE_PERCENT` in `src/lib/learn.ts`.
const REWRITE_PERCENT: usize = 60;

/// Field words outside the matched stretch up to which the field counts as
/// the paste plus a few words the user added. Past it the field is a
/// document, and the match must reach [`MIN_FOUND_PERCENT`]. Chosen from the
/// fields in `tests::the_location_thresholds_hold_on_their_corpus`: a
/// greeting and a sign-off around the paste add eleven words, and the
/// smallest document there adds more than three times as many.
const LOOSE_WORDS: usize = 11;

/// In a document, the share of the pasted words, in percent, that must be
/// found in order inside the matched stretch before it is taken to be the
/// paste. Chosen from the same fields: a document on the same subject that
/// does not hold the paste reaches two thirds of its words at best, and this
/// is the smallest share above that.
const MIN_FOUND_PERCENT: usize = 67;

/// The corrections found in `in_field`, the field's current text, against
/// `pasted`, the text the app put there. Pairs come back in the order they
/// occur in `pasted`, each distinct pair once, with both words exactly as
/// written apart from edge punctuation. A correction to a word in
/// `dictionary` is not reported.
///
/// Every case with nothing to learn returns an empty list; there is no error
/// path.
///
/// Cost: with `n` pasted words and `m` field words, time is O(n × m) and
/// memory O(n + m). Nothing proportional to n × m is ever held.
pub fn corrections_in(pasted: &str, in_field: &str, dictionary: &[String]) -> Vec<CorrectionPair> {
    let heard = words(pasted);
    let field = words(in_field);
    if heard.is_empty() || field.is_empty() {
        return Vec::new();
    }
    let (heard_ids, field_ids) = word_ids(&heard, &field);

    let span = locate(&heard_ids, &field_ids);
    // Zero cost means the paste is in the field unchanged.
    if span.cost == 0 || span.ambiguous {
        return Vec::new();
    }
    let typed = &field[span.start..span.end];
    let matched = common_words(&heard_ids, &field_ids[span.start..span.end]);
    let outside = field.len() - typed.len();
    if outside > LOOSE_WORDS && matched.len() * 100 < MIN_FOUND_PERCENT * heard.len() {
        return Vec::new();
    }

    let stretches = replaced_stretches(&matched, heard.len(), typed.len());
    let replaced: usize = stretches.iter().map(|s| s.heard.len()).sum();
    if is_rewrite(heard.len(), replaced) {
        return Vec::new();
    }

    let known: HashSet<String> = dictionary.iter().map(|w| canonical::fold(w)).collect();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    // A stretch with different word counts on each side has no reliable
    // word-to-word pairing, so only equal stretches are paired, in order.
    for stretch in stretches.iter().filter(|s| s.heard.len() == s.typed.len()) {
        let heard_words = &heard[stretch.heard.clone()];
        let typed_words = &typed[stretch.typed.clone()];
        for (&from, &to) in heard_words.iter().zip(typed_words) {
            if looks_like_a_fix(from, to, &known)
                && seen.insert((canonical::fold(from), canonical::fold(to)))
            {
                out.push(CorrectionPair {
                    from: from.to_string(),
                    to: to.to_string(),
                });
            }
        }
    }
    out
}

/// The words of `text`: the runs between whitespace, with every character
/// that is not a word character ([`belongs_in_word`]) trimmed from both ends,
/// and empty runs dropped. Punctuation inside a run stays, so contractions,
/// hyphenated words, dotted names and an underscore between two words are one
/// word each.
fn words(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .map(|run| run.trim_matches(|c: char| !belongs_in_word(c)))
        .filter(|word| !word.is_empty())
        .collect()
}

/// Both word lists as numbers that are equal exactly when the words are the
/// same word under [`canonical::fold`], so the alignment compares integers.
fn word_ids(heard: &[&str], field: &[&str]) -> (Vec<u32>, Vec<u32>) {
    let mut ids: HashMap<String, u32> = HashMap::new();
    let mut id_of = |word: &&str| {
        let next = ids.len() as u32;
        *ids.entry(canonical::fold(word)).or_insert(next)
    };
    let heard_ids = heard.iter().map(&mut id_of).collect();
    let field_ids = field.iter().map(&mut id_of).collect();
    (heard_ids, field_ids)
}

/// Where the paste sits in the field.
struct Span {
    /// The field words `start..end` are the best match for the paste.
    start: usize,
    end: usize,
    /// Word insertions, deletions and substitutions between that stretch
    /// and the paste.
    cost: usize,
    /// Another stretch that shares no word with this one matches as well.
    ambiguous: bool,
}

/// The stretch of `field` closest to `paste` by word edit distance, where
/// field words before and after the stretch cost nothing.
///
/// One row of costs is kept, with the field position each cheapest path
/// started from. On a tie a substitution is preferred to dropping a pasted
/// word, so a word the user replaced at either end of the paste stays inside
/// the stretch; among equally cheap ends the last one wins for the same
/// reason.
fn locate(paste: &[u32], field: &[u32]) -> Span {
    let m = field.len();
    let mut cost: Vec<usize> = vec![0; m + 1];
    let mut from: Vec<usize> = (0..=m).collect();
    let mut next_cost: Vec<usize> = vec![0; m + 1];
    let mut next_from: Vec<usize> = vec![0; m + 1];

    for (i, &word) in paste.iter().enumerate() {
        next_cost[0] = i + 1;
        next_from[0] = 0;
        for j in 1..=m {
            let mut best = cost[j - 1] + usize::from(word != field[j - 1]);
            let mut start = from[j - 1];
            if cost[j] + 1 < best {
                best = cost[j] + 1;
                start = from[j];
            }
            if next_cost[j - 1] + 1 < best {
                best = next_cost[j - 1] + 1;
                start = next_from[j - 1];
            }
            next_cost[j] = best;
            next_from[j] = start;
        }
        std::mem::swap(&mut cost, &mut next_cost);
        std::mem::swap(&mut from, &mut next_from);
    }

    let least = cost.iter().copied().min().unwrap_or(0);
    let end = (0..=m).rev().find(|&j| cost[j] == least).unwrap_or(0);
    let start = from[end];
    let ambiguous = (0..end).any(|j| cost[j] == least && from[j] < j && j <= start);
    Span {
        start,
        end,
        cost: least,
        ambiguous,
    }
}

/// Pasted words taken out at one place, and field words put in at that
/// same place.
struct Stretch {
    heard: Range<usize>,
    typed: Range<usize>,
}

/// The replaced stretches between consecutive matched words. A gap with
/// words on only one side is an addition or a deletion and is left out.
fn replaced_stretches(
    matched: &[(usize, usize)],
    heard_len: usize,
    typed_len: usize,
) -> Vec<Stretch> {
    let mut out = Vec::new();
    let (mut h, mut t) = (0, 0);
    for &(mh, mt) in matched.iter().chain(std::iter::once(&(heard_len, typed_len))) {
        if mh > h && mt > t {
            out.push(Stretch {
                heard: h..mh,
                typed: t..mt,
            });
        }
        h = mh + 1;
        t = mt + 1;
    }
    out
}

/// Whether replacing `replaced` of `pasted_words` words is a rewrite rather
/// than a fix.
fn is_rewrite(pasted_words: usize, replaced: usize) -> bool {
    replaced > REWRITE_FREE_WORDS && replaced * 100 > REWRITE_PERCENT * pasted_words
}

/// The per-pair tests: the corrected word is not already known, is a
/// different word, both words are long enough and close in spelling, and
/// neither is on [`NEVER_LEARNED`].
fn looks_like_a_fix(heard: &str, fixed: &str, known: &HashSet<String>) -> bool {
    let heard_key = canonical::fold(heard);
    let fixed_key = canonical::fold(fixed);
    !known.contains(&fixed_key)
        && heard_key != fixed_key
        && long_enough_fix(fixed)
        && heard_key.chars().count() >= MIN_HEARD_CHARS
        && !never_learned(heard)
        && !never_learned(fixed)
        && distance_ratio(heard, fixed) <= MAX_DISTANCE
}

/// Whether `word` is on [`NEVER_LEARNED`], in any case and whichever
/// apostrophe a keyboard or an autocorrect typed.
fn never_learned(word: &str) -> bool {
    let key = canonical::fold(word).replace(['\u{2018}', '\u{2019}', '\u{02BC}'], "'");
    NEVER_LEARNED.contains(&key.as_str())
}

fn long_enough_fix(word: &str) -> bool {
    canonical::fold(word).chars().count() >= MIN_FIXED_CHARS
}

/// Positions `(i, j)` of one longest common subsequence of `a` and `b`, in
/// order: the words the edit left in place.
///
/// Divide and conquer on `a`, so memory stays linear in the input while the
/// time is O(|a| × |b|).
fn common_words(a: &[u32], b: &[u32]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    match_into(a, b, 0, 0, &mut out);
    out
}

fn match_into(a: &[u32], b: &[u32], a_at: usize, b_at: usize, out: &mut Vec<(usize, usize)>) {
    // Equal leading and trailing words are always part of some longest
    // common subsequence, and an edit usually leaves most of both.
    let head = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    out.extend((0..head).map(|k| (a_at + k, b_at + k)));
    let (a, b) = (&a[head..], &b[head..]);
    let (a_at, b_at) = (a_at + head, b_at + head);
    let tail = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a, b) = (&a[..a.len() - tail], &b[..b.len() - tail]);

    if a.len() == 1 {
        if let Some(k) = b.iter().position(|&w| w == a[0]) {
            out.push((a_at, b_at + k));
        }
    } else if a.len() > 1 && !b.is_empty() {
        let half = a.len() / 2;
        let cut = best_cut(&a[..half], &a[half..], b);
        match_into(&a[..half], &b[..cut], a_at, b_at, out);
        match_into(&a[half..], &b[cut..], a_at + half, b_at + cut, out);
    }

    out.extend((0..tail).map(|k| (a_at + a.len() + k, b_at + b.len() + k)));
}

/// Where to split `b` so that `top` matched against `b[..cut]` and `bottom`
/// against `b[cut..]` together keep a longest common subsequence.
fn best_cut(top: &[u32], bottom: &[u32], b: &[u32]) -> usize {
    let forward = common_lengths(top, b);
    let bottom_rev: Vec<u32> = bottom.iter().rev().copied().collect();
    let b_rev: Vec<u32> = b.iter().rev().copied().collect();
    let backward = common_lengths(&bottom_rev, &b_rev);
    (0..=b.len())
        .max_by_key(|&cut| (forward[cut] + backward[b.len() - cut], Reverse(cut)))
        .unwrap_or(0)
}

/// `row[j]` is the length of the longest common subsequence of `a` and the
/// first `j` words of `b`.
fn common_lengths(a: &[u32], b: &[u32]) -> Vec<usize> {
    let mut row = vec![0usize; b.len() + 1];
    for &x in a {
        let mut diagonal = 0;
        for j in 1..=b.len() {
            let above = row[j];
            row[j] = if x == b[j - 1] {
                diagonal + 1
            } else {
                above.max(row[j - 1])
            };
            diagonal = above;
        }
    }
    row
}

/// Character edit distance between two words, scaled by the longer one to
/// 0..=1. Both terms are computed on the same [`canonical::fold`]ed strings,
/// counted in Unicode scalar values, so a case mapping that changes a word's
/// length cannot push the ratio past 1. Two empty words are 0 apart.
fn distance_ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = canonical::fold(a).chars().collect();
    let b: Vec<char> = canonical::fold(b).chars().collect();
    let longer = a.len().max(b.len());
    if longer == 0 {
        return 0.0;
    }
    char_edits(&a, &b) as f64 / longer as f64
}

/// Insertions, deletions and substitutions turning `a` into `b`.
fn char_edits(a: &[char], b: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, &x) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for j in 1..=b.len() {
            let above = row[j];
            row[j] = (diagonal + usize::from(x != b[j - 1]))
                .min(above + 1)
                .min(row[j - 1] + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Reverse;
    use unicode_normalization::char::is_combining_mark;

    fn dict(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn pairs(found: &[CorrectionPair]) -> Vec<(&str, &str)> {
        found
            .iter()
            .map(|p| (p.from.as_str(), p.to.as_str()))
            .collect()
    }

    fn learned(pasted: &str, field: &str) -> Vec<(String, String)> {
        corrections_in(pasted, field, &[])
            .into_iter()
            .map(|p| (p.from, p.to))
            .collect()
    }

    fn one(heard: &str, fixed: &str) -> Vec<(String, String)> {
        vec![(heard.to_string(), fixed.to_string())]
    }

    // ---------------------------------------------------------------------
    // Nothing to learn.
    // ---------------------------------------------------------------------

    #[test]
    fn nothing_is_learned_when_either_side_is_empty() {
        let text = "Please forward the invoice to Sidharth before lunch";
        assert!(learned("", text).is_empty(), "empty paste");
        assert!(learned(text, "").is_empty(), "empty field");
        assert!(learned("", "").is_empty(), "both empty");
        assert!(learned("   \n\t ", text).is_empty(), "a paste of nothing but spaces");
    }

    #[test]
    fn nothing_is_learned_when_the_text_is_untouched() {
        let text = "Please forward the invoice to Sidharth before lunch";
        assert!(learned(text, text).is_empty());
    }

    // ---------------------------------------------------------------------
    // The basic learn and its per-pair tests.
    // ---------------------------------------------------------------------

    #[test]
    fn a_misheard_name_corrected_in_place_is_learned() {
        assert_eq!(
            learned(
                "Please forward the invoice to Sidharth before lunch",
                "Please forward the invoice to Siddharth before lunch",
            ),
            one("Sidharth", "Siddharth")
        );
    }

    #[test]
    fn the_dictionary_check_folds_case_before_comparing() {
        let pasted = "Please forward the invoice to Sidharth before lunch";
        let field = "Please forward the invoice to Siddharth before lunch";
        for entry in ["Siddharth", "siddharth", "SIDDHARTH"] {
            let out = corrections_in(pasted, field, &dict(&["Koramangala", entry]));
            assert!(out.is_empty(), "{entry:?} in the dictionary still learned {out:?}");
        }
    }

    #[test]
    fn an_empty_dictionary_still_learns() {
        let out = corrections_in("Check the Soupabase logs", "Check the Supabase logs", &[]);
        assert_eq!(pairs(&out), vec![("Soupabase", "Supabase")]);
    }

    #[test]
    fn a_correction_shorter_than_four_characters_is_not_learned() {
        // One below the minimum: a three-letter name.
        assert!(learned("Ask Raaj to call", "Ask Raj to call").is_empty());
        // Exactly at it: a four-letter name.
        assert_eq!(learned("Ask Rawi to call", "Ask Ravi to call"), one("Rawi", "Ravi"));
    }

    #[test]
    fn a_case_only_change_is_not_a_correction() {
        assert!(learned(
            "please ask sidharth about the kubernetes rollout",
            "Please ask Sidharth about the Kubernetes Rollout",
        )
        .is_empty());
    }

    #[test]
    fn observed_casing_is_preserved_on_both_sides_of_the_pair() {
        assert_eq!(learned("ping Sidharth now", "ping SIDDHARTH now"), one("Sidharth", "SIDDHARTH"));
        assert_eq!(
            learned("the SIDHARTH account is locked", "the SIDDHARTH account is locked"),
            one("SIDHARTH", "SIDDHARTH")
        );
    }

    #[test]
    fn edge_punctuation_is_not_a_substitution() {
        assert_eq!(
            learned("Tell Pria the build passed", "Tell Priya, the build passed."),
            one("Pria", "Priya")
        );
    }

    #[test]
    fn one_name_fixed_at_two_places_gives_one_pair() {
        assert_eq!(
            learned(
                "Kaavya aaj aayegi aur Kaavya kal jaayegi",
                "Kavya aaj aayegi aur Kavya kal jaayegi",
            ),
            one("Kaavya", "Kavya")
        );
    }

    #[test]
    fn two_different_mishearings_of_the_same_word_both_survive_one_batch() {
        assert_eq!(
            learned(
                "Sidharth and Sidarth are the same person",
                "Siddharth and Siddharth are the same person",
            ),
            vec![
                ("Sidharth".to_string(), "Siddharth".to_string()),
                ("Sidarth".to_string(), "Siddharth".to_string()),
            ]
        );
    }

    #[test]
    fn a_rejected_pair_does_not_shadow_a_later_acceptable_one() {
        // Mohan -> Siddharth is a different name, not a fix; the later
        // Sidharth -> Siddharth is a fix to the same word and must survive.
        assert_eq!(
            learned("ask Mohan and Sidharth", "ask Siddharth and Siddharth"),
            one("Sidharth", "Siddharth")
        );
    }

    // ---------------------------------------------------------------------
    // What counts as a substitution.
    // ---------------------------------------------------------------------

    #[test]
    fn adding_or_removing_words_anywhere_teaches_nothing() {
        let base = "send the Kubernetes notes today";
        for field in [
            "please send the Kubernetes notes today",
            "send the new Kubernetes notes today",
            "send the Kubernetes notes today thanks",
            "the Kubernetes notes today",
            "send the notes today",
            "send the Kubernetes notes",
        ] {
            assert!(learned(base, field).is_empty(), "{field:?}");
            assert!(learned(field, base).is_empty(), "{field:?} reversed");
        }
    }

    #[test]
    fn a_word_replaced_in_place_is_one_pair() {
        assert_eq!(
            learned("send the Kubernetis notes today", "send the Kubernetes notes today"),
            one("Kubernetis", "Kubernetes")
        );
    }

    #[test]
    fn an_equal_length_replaced_stretch_pairs_word_by_word() {
        assert_eq!(
            learned("tell Shrinivas Venkatest tomorrow", "tell Srinivas Venkatesh tomorrow"),
            vec![
                ("Shrinivas".to_string(), "Srinivas".to_string()),
                ("Venkatest".to_string(), "Venkatesh".to_string()),
            ]
        );
    }

    #[test]
    fn an_unequal_length_replaced_stretch_teaches_nothing() {
        // Two heard words became one: the engine cannot say which of them
        // the new word replaces, so it says nothing.
        assert!(learned("ask Sid Harth to call", "ask Siddharth to call").is_empty());
        // One heard word became two.
        assert!(learned("we use Postgress daily", "we use Postgres databases daily").is_empty());
    }

    // ---------------------------------------------------------------------
    // The rewrite test.
    // ---------------------------------------------------------------------

    #[test]
    fn a_wholesale_rewrite_teaches_nothing() {
        // Pria -> Priya and Kubernetis -> Kubernetes would each pass on their
        // own; the edit as a whole is a new sentence.
        assert!(learned(
            "Tell Pria the Kubernetis demo moved to Thursday",
            "Ask Priya whether our Kubernetes walkthrough could happen Friday",
        )
        .is_empty());
    }

    const TEN_NAMES: &str = "Pria Sidharth Shrinivas Venkatest Gourav Kartik Dipak met the hires";

    #[test]
    fn the_most_replaced_words_a_fix_may_have_still_learns() {
        // Six of ten words replaced: exactly the threshold, which still passes.
        let field = "Priya Siddharth Srinivas Venkatesh Gaurav Karthik Dipak met the hires";
        assert_eq!(learned(TEN_NAMES, field).len(), 6);
        // Two of three: the free allowance, which a three-word paste needs.
        assert_eq!(learned("Kavita Menen called", "Kavitha Menon called").len(), 2);
    }

    #[test]
    fn one_more_replaced_word_drops_the_whole_batch() {
        // Seven of ten: every pair is valid on its own, and none survives.
        let field = "Priya Siddharth Srinivas Venkatesh Gaurav Karthik Deepak met the hires";
        assert!(learned(TEN_NAMES, field).is_empty());
        // Three of three.
        assert!(learned("Kavita Menen Dipak", "Kavitha Menon Deepak").is_empty());
    }

    // ---------------------------------------------------------------------
    // Finding the paste in the field.
    // ---------------------------------------------------------------------

    const LETTER_START: &str = "Hi all, thanks for the update yesterday. I went through the \
        notes from the planning session and most of it looks right to me, although the \
        timeline for the mobile release still feels tight. A couple of things from my side.";
    const LETTER_END: &str = "Let me know if anything here is unclear or if you want to go \
        through it on a call. I am around most of the afternoon and can move things if needed.";

    /// Shares names and phrases with the pastes below without containing any
    /// of them, which is what a document on the same subject looks like.
    const SAME_SUBJECT: &str = "Quick update on the cluster work. Siddharth finished the \
        database backups last night and Priya has the dashboards open, so we are in good \
        shape. The Kubernetes upgrade itself still needs a date; Thursday is possible but \
        Wednesday is out because Siddharth is travelling. Please send any notes before the \
        standup and ask Priya if you want to call about it. Thanks, and message me tomorrow \
        if anything changes.";

    fn in_letter(text: &str) -> String {
        format!("{LETTER_START} {text} {LETTER_END}")
    }

    #[test]
    fn a_field_barely_larger_than_the_paste_is_taken_whole() {
        assert_eq!(
            learned("Sidharth will review it", "Hi Sam, Siddharth will review it, thanks"),
            one("Sidharth", "Siddharth")
        );
    }

    #[test]
    fn a_document_containing_the_paste_verbatim_reports_no_edit() {
        let paste = "Pria will review the Kubernetis upgrade on Thursday";
        assert!(learned(paste, &in_letter(paste)).is_empty());
        // Even when the user fixed a word somewhere else in the document.
        let elsewhere = in_letter(paste).replace("planning session", "planning meeting");
        assert!(learned(paste, &elsewhere).is_empty());
    }

    #[test]
    fn a_correction_inside_a_long_document_is_still_located() {
        let paste = "Pria will review the Kubernetis upgrade on Thursday";
        let field = in_letter("Pria will review the Kubernetes upgrade on Thursday");
        assert_eq!(learned(paste, &field), one("Kubernetis", "Kubernetes"));
    }

    #[test]
    fn a_fix_at_either_edge_of_the_paste_is_found_inside_a_document() {
        let paste = "Pria will review the Kubernetis upgrade";
        let first = in_letter("Priya will review the Kubernetis upgrade");
        assert_eq!(learned(paste, &first), one("Pria", "Priya"));
        let last = in_letter("Pria will review the Kubernetes upgrade");
        assert_eq!(learned(paste, &last), one("Kubernetis", "Kubernetes"));
    }

    #[test]
    fn a_field_that_does_not_contain_the_paste_teaches_nothing() {
        for paste in [
            "message Sidharth tomorrow",
            "please send the Kubernetis notes to Sidharth before the standup",
            "ask Pria to call about the Kubernetis upgrade",
        ] {
            let out = learned(paste, SAME_SUBJECT);
            assert!(out.is_empty(), "{paste:?} found where it is not: {out:?}");
            let out = learned(paste, &in_letter(SAME_SUBJECT));
            assert!(out.is_empty(), "{paste:?} found where it is not: {out:?}");
        }
    }

    #[test]
    fn a_paste_that_could_be_in_two_places_teaches_nothing() {
        let paste = "please thank Pria today";
        let twice = format!(
            "{LETTER_START} please thank Priya today {LETTER_END} please thank Priya today"
        );
        assert!(learned(paste, &twice).is_empty());
    }

    // ---------------------------------------------------------------------
    // Robustness and cost.
    // ---------------------------------------------------------------------

    #[test]
    fn hostile_and_degenerate_inputs_do_not_panic() {
        let long_word = "क".repeat(5_000);
        let cases: Vec<(String, String)> = vec![
            (" ".into(), "\t\n".into()),
            ("...".into(), ",,, !!!".into()),
            ("। ।। ॥".into(), "।।।।".into()),
            ("\u{093C}".into(), "\u{094D}\u{093C}".into()),
            ("🙂 🙂".into(), "🙂🙂 🙂".into()),
            ("a🙂b".into(), "a🙂🙂b".into()),
            ("\u{0}\u{0}".into(), "\u{200D}\u{200C}".into()),
            ("İİİ".into(), "iii".into()),
            ("ẞ".into(), "SS".into()),
            (long_word.clone(), format!("{long_word}ख")),
            ("one".into(), "one two three four five six seven eight nine ten".into()),
            ("one two three four five six seven eight nine ten".into(), "one".into()),
        ];
        for (pasted, field) in &cases {
            let _ = corrections_in(pasted, field, &[]);
            let _ = corrections_in(field, pasted, &dict(&["", " ", "।"]));
        }

        // A Latin-script name inside Devanagari text is still learned.
        assert_eq!(
            learned("मैंने Sidharth को फ़ोन किया", "मैंने Siddharth को फ़ोन किया"),
            one("Sidharth", "Siddharth")
        );
    }

    #[test]
    fn the_distance_ratio_normalizes_both_of_its_terms_the_same_way() {
        // Dotted capital I lowercases to two scalars. Folding only one term,
        // or counting the length on the unfolded word, would give 1.0 here or
        // divide by a length the distance never saw.
        let dotted = distance_ratio("İstanbul", "Istanbul");
        assert!((0.0..=1.0).contains(&dotted), "{dotted}");
        assert_eq!(dotted, 1.0 / 9.0);
        assert_eq!(distance_ratio("İ", "I"), 0.5);
        // Capital sharp s folds to the small one: the same word.
        assert_eq!(distance_ratio("GROẞ", "groß"), 0.0);

        assert_eq!(distance_ratio("Siddharth", "Siddharth"), 0.0);
        assert_eq!(distance_ratio("SIDDHARTH", "siddharth"), 0.0);
        assert_eq!(distance_ratio("", ""), 0.0);
        assert_eq!(distance_ratio("", "Sid"), 1.0);

        // Values from the similarity corpus.
        assert_eq!(distance_ratio("Sidharth", "Siddharth"), 1.0 / 9.0);
        assert_eq!(distance_ratio("अछी", "अच्छी"), 2.0 / 5.0);
        assert_eq!(distance_ratio("Laxmi", "Lakshmi"), 3.0 / 7.0);
        // Precomposed and decomposed nukta spellings are one word.
        assert_eq!(distance_ratio("\u{0958}लम", "\u{0915}\u{093C}लम"), 0.0);
    }

    /// The monitor stops before calling the engine on a field past its
    /// limit, so a field at the limit made of one-letter words is the most
    /// words the engine can ever be handed, on both sides at once.
    #[test]
    fn a_field_at_the_monitor_limit_of_one_letter_words_finishes() {
        const LIMIT_CHARS: usize = 10_000;
        let letters = |seed: u32| {
            let mut state = seed;
            let mut out = String::with_capacity(LIMIT_CHARS);
            while out.len() + 2 <= LIMIT_CHARS {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                out.push(char::from(b'a' + ((state >> 16) % 4) as u8));
                out.push(' ');
            }
            out
        };
        let pasted = letters(7);
        let field = letters(11);
        assert!(field.chars().count() <= LIMIT_CHARS);
        let _ = corrections_in(&pasted, &field, &[]);
        let _ = corrections_in(&pasted, &format!("{pasted}x"), &[]);
    }

    // ---------------------------------------------------------------------
    // The corpora the thresholds were chosen from.
    // ---------------------------------------------------------------------

    /// Corrected words a user types when not fixing a mishearing. Every one
    /// of three characters or fewer is rejected by the length test, and every
    /// one of them is on `NEVER_LEARNED` as well.
    const NOT_FIXES: &[&str] = &[
        "a", "an", "the", "I", "me", "my", "he", "she", "him", "her", "his", "it", "its", "we",
        "us", "our", "you", "and", "but", "or", "of", "to", "in", "on", "at", "is", "be", "by",
        "if", "so", "do", "no", "up", "as", "am", "are", "was", "for", "not", "I'm", "I'd",
        "they", "them", "your", "it's", "that", "this", "then", "than", "with", "from",
        "don't", "we're", "they're",
    ];

    /// Genuine short fixes: short names, short technical terms and short
    /// Hindi words written in Devanagari.
    const SHORT_FIXES: &[&str] = &[
        "Ravi", "Amit", "Neha", "Arun", "Anil", "Zoya", "Riya", "Deno", "Vite", "JSON",
        "नहीं", "\u{0915}\u{093C}लम", "\u{091C}\u{093C}रा", "पैसा",
        "Raj", "Dev", "Jai", "npm", "ठीक", "कलम", "मैं",
    ];

    fn chars_of(word: &str) -> usize {
        canonical::fold(word).chars().count()
    }

    #[test]
    fn the_fixed_word_minimum_holds_on_its_corpus() {
        // The kept Devanagari tests learn this four-scalar corrected word,
        // and it sets the minimum.
        assert_eq!(chars_of("\u{0915}\u{093C}लम"), MIN_FIXED_CHARS);

        for word in NOT_FIXES.iter().filter(|w| chars_of(w) <= 3) {
            assert!(!long_enough_fix(word), "{word:?} passed the length test");
        }
        for word in NOT_FIXES {
            assert!(never_learned(word), "{word:?} is not on NEVER_LEARNED");
        }
        for word in SHORT_FIXES.iter().filter(|w| chars_of(w) >= 4) {
            assert!(long_enough_fix(word), "{word:?} failed the length test");
        }
        // The cost of four: three-scalar names and Hindi words are not learned.
        for word in ["Raj", "Dev", "Jai", "npm", "ठीक", "कलम", "मैं"] {
            assert!(!long_enough_fix(word), "{word:?}");
        }
    }

    /// What speech recognition wrote, and the user's fix: names in Latin
    /// script, technical terms, and Hindi in Devanagari.
    const MISHEARINGS: &[(&str, &str)] = &[
        ("Sidharth", "Siddharth"),
        ("Pria", "Priya"),
        ("Shriya", "Shreya"),
        ("Dipak", "Deepak"),
        ("Ashwarya", "Aishwarya"),
        ("Angeli", "Anjali"),
        ("Raul", "Rahul"),
        ("Shrinivas", "Srinivas"),
        ("Laxmi", "Lakshmi"),
        ("Gourav", "Gaurav"),
        ("Kartik", "Karthik"),
        ("Bawna", "Bhavna"),
        ("Venkatest", "Venkatesh"),
        ("Kavita", "Kavitha"),
        ("Anoushka", "Anushka"),
        ("Chetanya", "Chaitanya"),
        ("Ritik", "Hrithik"),
        ("Soya", "Zoya"),
        ("Tanvee", "Tanvi"),
        ("Kubernetis", "Kubernetes"),
        ("Postgres", "PostgreSQL"),
        ("Soupabase", "Supabase"),
        ("Svelt", "Svelte"),
        ("engine", "nginx"),
        ("Cotlin", "Kotlin"),
        ("Jason", "JSON"),
        ("Sequelite", "SQLite"),
        ("Tailwing", "Tailwind"),
        ("Sarwam", "Sarvam"),
        ("Doctor", "Docker"),
        ("Versel", "Vercel"),
        ("Radish", "Redis"),
        ("अछी", "अच्छी"),
        ("कलम", "\u{0915}\u{093C}लम"),
        ("नही", "नहीं"),
        ("जरूर", "\u{091C}\u{093C}रूर"),
        ("क्रिपया", "कृपया"),
        ("धन्यबाद", "धन्यवाद"),
        ("सकूल", "स्कूल"),
        ("बहोत", "बहुत"),
        ("पडेगा", "प\u{0921}\u{093C}ेगा"),
    ];

    /// A different word the user chose while rewording: synonyms and
    /// different nouns, in English and Hindi.
    const REWORDINGS: &[(&str, &str)] = &[
        ("big", "large"),
        ("small", "little"),
        ("quick", "fast"),
        ("start", "begin"),
        ("buy", "purchase"),
        ("help", "assist"),
        ("show", "display"),
        ("fix", "repair"),
        ("send", "forward"),
        ("call", "phone"),
        ("car", "vehicle"),
        ("job", "work"),
        ("meeting", "call"),
        ("report", "summary"),
        ("email", "message"),
        ("idea", "plan"),
        ("problem", "issue"),
        ("happy", "glad"),
        ("tomorrow", "today"),
        ("Monday", "Friday"),
        ("team", "group"),
        ("answer", "reply"),
        ("error", "bug"),
        ("laptop", "computer"),
        ("apple", "orange"),
        ("river", "mountain"),
        ("Delhi", "Mumbai"),
        ("काम", "कार्य"),
        ("घर", "मकान"),
        ("दोस्त", "मित्र"),
        ("किताब", "पुस्तक"),
        ("जल्दी", "तुरंत"),
        ("बड़ा", "विशाल"),
    ];

    /// Mishearings spelled too differently from their fix for any ceiling
    /// that still rejects every pair in [`REWORDINGS`].
    const MISHEARINGS_OUT_OF_REACH: &[(&str, &str)] = &[
        ("Nickel", "Nikhil"),
        ("Tory", "Tauri"),
        ("Searsha", "Saoirse"),
        ("Shivon", "Siobhan"),
        ("Wakeen", "Joaquin"),
        ("देहली", "दिल्ली"),
    ];

    /// Rewordings as close in spelling as fixes the ceiling has to keep, so
    /// no ceiling rejects them. They are observed, and only the two-session
    /// guard keeps them from becoming rules.
    const REWORDINGS_OUT_OF_REACH: &[(&str, &str)] = &[("Tuesday", "Thursday"), ("house", "home")];

    /// The pairs `corrections_in` reports when `heard` is fixed to
    /// `fixed` in an ordinary sentence.
    fn learned_in_a_sentence(heard: &str, fixed: &str) -> Vec<(String, String)> {
        learned(
            &format!("please tell {heard} about it today"),
            &format!("please tell {fixed} about it today"),
        )
    }

    #[test]
    fn the_similarity_ceiling_holds_on_its_corpus() {
        // The kept Devanagari tests learn अछी -> अच्छी, at 0.40, so the
        // ceiling cannot be lower than that.
        let floor = 40;
        let within = |&(a, b): &(&str, &str), hundredths: u32| {
            distance_ratio(a, b) <= f64::from(hundredths) / 100.0
        };
        for pair in REWORDINGS_OUT_OF_REACH {
            assert!(within(pair, floor), "{pair:?} is not out of reach");
        }

        // Among the ceilings that reject every rewording, the one that keeps
        // the most mishearings; the smallest of those.
        let all_mishearings = || MISHEARINGS.iter().chain(MISHEARINGS_OUT_OF_REACH);
        let chosen = (floor..=100)
            .filter(|&c| REWORDINGS.iter().all(|pair| !within(pair, c)))
            .max_by_key(|&c| (all_mishearings().filter(|pair| within(pair, c)).count(), Reverse(c)))
            .unwrap();
        assert_eq!(f64::from(chosen) / 100.0, MAX_DISTANCE);

        for &(heard, fixed) in MISHEARINGS {
            assert_eq!(learned_in_a_sentence(heard, fixed), one(heard, fixed), "{heard} -> {fixed}");
        }
        for &(heard, fixed) in REWORDINGS {
            assert!(learned_in_a_sentence(heard, fixed).is_empty(), "{heard} -> {fixed}");
        }
        for &(heard, fixed) in MISHEARINGS_OUT_OF_REACH {
            assert!(learned_in_a_sentence(heard, fixed).is_empty(), "{heard} -> {fixed}");
        }
        for &(heard, fixed) in REWORDINGS_OUT_OF_REACH {
            assert_eq!(learned_in_a_sentence(heard, fixed), one(heard, fixed), "{heard} -> {fixed}");
        }
    }

    /// Edits labelled fix (1 to 3 words changed) or rewrite, against pastes
    /// of 1, 2, 3, 5, 10 and 30 words.
    const REWRITE_CORPUS: &[(bool, &str, &str)] = &[
        (false, "Sidharth", "Siddharth"),
        (false, "Kavita Menen", "Kavitha Menon"),
        (false, "message Sidharth tomorrow", "message Siddharth tomorrow"),
        (false, "Kavita Menen called", "Kavitha Menon called"),
        (true, "send it now", "forward this today"),
        (true, "sounds good thanks", "that works cheers"),
        (false, "please ask Pria to call", "please ask Priya to call"),
        (false, "Shriya and Dipak are here", "Shreya and Deepak are here"),
        (false, "Shrinivas and Kavita use Kubernetis", "Srinivas and Kavitha use Kubernetes"),
        (true, "can we meet on Monday", "could we catch up Tuesday"),
        (true, "I will call you later", "let me ring you tonight"),
        (true, "let's catch up on Friday", "can we talk this Thursday"),
        (
            false,
            "please send the Kubernetis notes to Sidharth before the standup",
            "please send the Kubernetes notes to Siddharth before the standup",
        ),
        (
            false,
            "Pria said Sidharth moved the Kubernetis demo to the afternoon",
            "Priya said Siddharth moved the Kubernetes demo to the afternoon",
        ),
        (
            true,
            "please send me the final report before the meeting tomorrow",
            "can you share the finished summary ahead of tomorrow's call",
        ),
        (
            true,
            "I think we should move the launch to next week",
            "maybe we could push the release back a week",
        ),
        (
            true,
            "the new build fixes the crash when you open settings",
            "this version stops the app from crashing in settings",
        ),
        (
            false,
            "the Kubernetis upgrade is scheduled for Thursday evening and Sidharth will handle \
             the database backups while Pria watches the dashboards so please avoid deploying \
             anything after six until we confirm",
            "the Kubernetes upgrade is scheduled for Thursday evening and Siddharth will handle \
             the database backups while Priya watches the dashboards so please avoid deploying \
             anything after six until we confirm",
        ),
        (
            true,
            "the Kubernetis upgrade is scheduled for Thursday evening and Sidharth will handle \
             the database backups while Pria watches the dashboards so please avoid deploying \
             anything after six until we confirm",
            "we are upgrading the cluster on Thursday night so Siddharth is taking care of \
             backups while Priya monitors things and nobody should deploy after 6 pm until we \
             give the all clear",
        ),
        (
            true,
            "the Kubernetis upgrade is scheduled for Thursday evening and Sidharth will handle \
             the database backups while Pria watches the dashboards so please avoid deploying \
             anything after six until we confirm",
            "quick note for everyone: Thursday evening brings the cluster upgrade, backups are \
             with Siddharth, dashboards with Priya, and no deploys past six please until you \
             hear from us",
        ),
    ];

    /// Pasted words and how many of them were replaced in place, as the
    /// engine counts them.
    fn replaced_words(pasted: &str, field: &str) -> (usize, usize) {
        let heard = words(pasted);
        let typed = words(field);
        let (heard_ids, typed_ids) = word_ids(&heard, &typed);
        let span = locate(&heard_ids, &typed_ids);
        let matched = common_words(&heard_ids, &typed_ids[span.start..span.end]);
        let replaced = replaced_stretches(&matched, heard.len(), span.end - span.start)
            .iter()
            .map(|s| s.heard.len())
            .sum();
        (heard.len(), replaced)
    }

    #[test]
    fn the_rewrite_threshold_holds_on_its_corpus() {
        let measured: Vec<(bool, usize, usize)> = REWRITE_CORPUS
            .iter()
            .map(|&(rewrite, pasted, field)| {
                let (n, replaced) = replaced_words(pasted, field);
                (rewrite, n, replaced)
            })
            .collect();
        let fits = |free: usize, percent: usize| {
            measured.iter().all(|&(rewrite, n, replaced)| {
                (replaced > free && replaced * 100 > percent * n) == rewrite
            })
        };
        // Exactly one allowance works for every paste length, and the
        // threshold is the smallest whole percent that fits with it.
        let allowances: Vec<usize> = (0..=5).filter(|&f| (1..=100).any(|p| fits(f, p))).collect();
        assert_eq!(allowances, vec![REWRITE_FREE_WORDS]);
        let smallest = (1..=100).find(|&p| fits(REWRITE_FREE_WORDS, p)).unwrap();
        assert_eq!(smallest, REWRITE_PERCENT);

        for &(rewrite, pasted, field) in REWRITE_CORPUS {
            assert_eq!(learned(pasted, field).is_empty(), rewrite, "{pasted:?} -> {field:?}");
        }
    }

    /// How the engine sees a field: words outside the stretch it matched,
    /// share of the paste found (in percent), and whether the match was
    /// ambiguous.
    fn located(pasted: &str, field: &str) -> (usize, usize, bool) {
        let heard = words(pasted);
        let typed = words(field);
        let (heard_ids, typed_ids) = word_ids(&heard, &typed);
        let span = locate(&heard_ids, &typed_ids);
        let matched = common_words(&heard_ids, &typed_ids[span.start..span.end]);
        (
            typed.len() - (span.end - span.start),
            matched.len() * 100 / heard.len(),
            span.ambiguous,
        )
    }

    #[test]
    fn the_location_thresholds_hold_on_their_corpus() {
        let pastes = [
            ("message Sidharth tomorrow", "message Siddharth tomorrow"),
            ("please thank Pria today", "please thank Priya today"),
            ("please ask Pria to call", "please ask Priya to call"),
            ("send the Kubernetis notes to Sidharth", "send the Kubernetes notes to Siddharth"),
            (
                "Pria will review the Kubernetis upgrade on Thursday",
                "Priya will review the Kubernetes upgrade on Thursday",
            ),
            (
                "please send the Kubernetis notes to Sidharth before the standup",
                "please send the Kubernetes notes to Siddharth before the standup",
            ),
            (
                "the Kubernetis upgrade moved to Thursday because Sidharth is out on Wednesday \
                 and Pria needs time to check backups first",
                "the Kubernetes upgrade moved to Thursday because Siddharth is out on Wednesday \
                 and Priya needs time to check backups first",
            ),
        ];
        let sign_off = "Let me know if you have any questions. Thanks,";

        let mut added_most = 0;
        let mut document_least = usize::MAX;
        let mut absent_most = 0;
        let mut unplaced = Vec::new();
        for (pasted, fixed) in pastes {
            let added = [
                fixed.to_string(),
                format!("{fixed} thanks!"),
                format!("Hi team, {fixed}"),
                format!("{fixed} {sign_off}"),
                format!("Hi Sam, {fixed} {sign_off}"),
            ];
            for field in &added {
                added_most = added_most.max(located(pasted, field).0);
                assert!(!learned(pasted, field).is_empty(), "{field:?}");
            }
            for field in [in_letter(fixed), format!("{LETTER_START} {fixed}")] {
                let (outside, found, _) = located(pasted, &field);
                document_least = document_least.min(outside);
                let believed = found >= MIN_FOUND_PERCENT;
                assert_eq!(!learned(pasted, &field).is_empty(), believed, "{field:?}");
                if !believed && !unplaced.contains(&pasted) {
                    unplaced.push(pasted);
                }
            }
            for field in [SAME_SUBJECT.to_string(), in_letter(SAME_SUBJECT)] {
                absent_most = absent_most.max(located(pasted, &field).1);
                assert!(learned(pasted, &field).is_empty(), "{pasted:?} in {field:?}");
            }
        }
        // The loosest field that is still "the paste with words added" sets
        // the cut-off; the tightest document is far above it.
        assert_eq!(added_most, LOOSE_WORDS);
        assert!(document_least > 3 * LOOSE_WORDS, "{document_least}");
        // The best share any field without the paste reaches sets the floor:
        // the smallest whole percent above it.
        assert_eq!(absent_most + 1, MIN_FOUND_PERCENT);
        // The price of failing closed: a fixed paste that keeps two thirds of
        // its words or fewer scores no better than a document that only
        // shares words with it, so inside a document these are not learned.
        assert_eq!(
            unplaced,
            vec!["message Sidharth tomorrow", "send the Kubernetis notes to Sidharth"]
        );
    }

    // ---------------------------------------------------------------------
    // Devanagari.
    // ---------------------------------------------------------------------

    /// The dictionary is text the user typed; the edited field arrives
    /// NFC-normalized from the monitor, and NFC spells `क़` apart. A word the
    /// user already has must suppress the learn in either spelling, or the
    /// same correction is re-learned forever.
    #[test]
    fn a_dictionary_word_suppresses_the_learn_in_either_spelling() {
        // कलम → क़लम, the corrected word spelled apart (what NFC produces).
        let pasted = "मुझे कलम चाहिए";
        let in_field = "मुझे \u{0915}\u{093C}लम चाहिए";
        assert_eq!(
            pairs(&corrections_in(pasted, in_field, &[])),
            vec![("कलम", "\u{0915}\u{093C}लम")],
            "the control: with no dictionary it is learned"
        );

        // The user's dictionary holds the precomposed spelling.
        let out = corrections_in(pasted, in_field, &dict(&["\u{0958}लम"]));
        assert!(out.is_empty(), "learned a word the user already has: {out:?}");
    }

    /// Adding or removing words teaches nothing at all. Without this, a user
    /// who deletes a clause would have the word before it "corrected" to
    /// whatever follows.
    #[test]
    fn pure_insertions_and_pure_deletions_yield_no_pairs() {
        let short = "the quick brown fox jumps";
        let long = "the very quick brown fox jumps";

        let inserted = corrections_in(short, long, &[]);
        assert!(inserted.is_empty(), "an insertion produced {inserted:?}");

        let deleted = corrections_in(long, short, &[]);
        assert!(deleted.is_empty(), "a deletion produced {deleted:?}");
    }

    /// Which of "its" and "it's", "then" and "than" is right depends on the
    /// sentence, so a fix between them teaches nothing about the next one. A
    /// real fix beside one still learns.
    #[test]
    fn a_homophone_or_function_word_fix_is_never_learned() {
        for (pasted, field) in [
            ("The company changed its logo today", "The company changed it's logo today"),
            ("The company changed it's logo today", "The company changed its logo today"),
            ("It is better then ever before", "It is better than ever before"),
            ("Thanks, your welcome to join", "Thanks, you're welcome to join"),
            ("Their going home early tonight", "They're going home early tonight"),
            ("We parked there car outside", "We parked their car outside"),
            ("Were going to the office", "We're going to the office"),
            ("Whose coming to dinner tonight", "Who's coming to dinner tonight"),
            ("I will loose the keys again", "I will lose the keys again"),
            ("The weather was nice either way", "The whether was nice either way"),
            ("They changed its logo", "They changed it\u{2019}s logo"),
            ("Order the steal beams today", "Order the steel beams today"),
            ("Thank you for your patients today", "Thank you for your patience today"),
            ("Renew the license next month", "Renew the licence next month"),
            ("Ask the personal team about it", "Ask the personnel team about it"),
            ("We skipped desert after lunch", "We skipped dessert after lunch"),
        ] {
            let out = learned(pasted, field);
            assert!(out.is_empty(), "{pasted:?} -> {field:?} learned {out:?}");
        }
        assert_eq!(learned("Ask Pria if its ready", "Ask Priya if it's ready"), one("Pria", "Priya"));
    }

    /// An entry not in the form `never_learned` looks words up in could
    /// never match anything.
    #[test]
    fn the_never_learned_list_is_in_lookup_form() {
        for word in NEVER_LEARNED {
            assert_eq!(canonical::fold(word), *word, "{word:?}");
            assert!(!word.contains(['\u{2018}', '\u{2019}', '\u{02BC}']), "{word:?}");
        }
    }

    /// Pins the list against `src/lib/learn.ts`: its length, its two ends,
    /// and entries from every part of it, looked up the way the Home learner
    /// looks them up (composed, lowercased, curly apostrophes made straight).
    #[test]
    fn the_never_learned_list_matches_the_home_learners() {
        assert_eq!(NEVER_LEARNED.len(), 326);
        assert_eq!(NEVER_LEARNED.first(), Some(&"a"));
        assert_eq!(NEVER_LEARNED.last(), Some(&"herd"));
        for word in [
            // Articles, determiners and quantifiers.
            "The", "those", "neither", "own",
            // Pronouns and possessives.
            "I", "Their", "themselves", "whose", "one",
            // Contractions, with each apostrophe a keyboard may type.
            "it's", "IT\u{2019}S", "they\u{2018}re", "who\u{02BC}s", "hadn't",
            // Auxiliary and modal verbs.
            "am", "been", "Could", "must",
            // Prepositions.
            "of", "onto", "towards", "via",
            // Conjunctions.
            "and", "although", "whether", "than",
            // Adverbs that work like function words.
            "then", "There", "only",
            // Homophones.
            "two", "hour", "Write", "weather", "loose", "piece", "stationary",
            "Meat", "dessert", "Personnel", "scent", "cent", "practise", "herd",
        ] {
            assert!(never_learned(word), "{word:?} is not on the list");
        }
        for word in ["Siddharth", "Kubernetes", "Receive", "नहीं", "whole-hearted"] {
            assert!(!never_learned(word), "{word:?} is on the list");
        }
    }

    /// A two-character original is not a safe rule trigger even when the
    /// correction itself looks fine.
    #[test]
    fn a_two_character_original_is_not_learned_from() {
        let out = corrections_in("I said he was here", "I said the was here", &[]);
        assert!(out.is_empty(), "learned a rule for a two-letter word: {out:?}");
    }

    /// "अछी" → "अच्छी" is a real spelling correction (two inserted chars out
    /// of five, 0.40). The corrected token is written with a trailing danda
    /// AND ends in a vowel sign, which is exactly where a Latin-shaped
    /// tokenizer amputates: strip everything that is not a letter from the
    /// end and you get "अच्छ".
    #[test]
    fn a_devanagari_correction_survives_with_its_clusters_intact() {
        let out = corrections_in(
            "मुझे यह किताब बहुत अछी।",
            "मुझे यह किताब बहुत अच्छी।",
            &[],
        );
        assert_eq!(pairs(&out), vec![("अछी", "अच्छी")]);

        for p in &out {
            for word in [&p.from, &p.to] {
                assert!(
                    !word.chars().next().is_some_and(is_combining_mark),
                    "{word:?} starts with an orphaned combining mark"
                );
                assert!(
                    !word.ends_with('।') && !word.ends_with(','),
                    "{word:?} kept edge punctuation"
                );
            }
        }
    }

    /// The tokenizer, directly: a trailing matra, nukta or halant is part of
    /// the word and must not be trimmed with the punctuation around it.
    #[test]
    fn edge_stripping_keeps_devanagari_combining_marks_attached() {
        assert_eq!(words("अच्छी।"), vec!["अच्छी"]);
        assert_eq!(words("साज़,"), vec!["साज़"]);
        assert_eq!(words("(सम्)"), vec!["सम्"]);
        // ASCII edge punctuation is trimmed the same way.
        assert_eq!(words("\"hello,\" world."), vec!["hello", "world"]);
        assert_eq!(words("don't co-op foo.bar"), vec!["don't", "co-op", "foo.bar"]);
    }
}
