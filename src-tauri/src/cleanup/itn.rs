//! Inverse text normalization: spoken numbers, times, dates, percentages and
//! currency → written forms. Style rule: one–nine stay as words unless they
//! carry a unit ("three cats" stays, "three thirty pm" → 3:30 PM).

fn norm(token: &str) -> String {
    token
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .to_lowercase()
}

/// Trailing punctuation of a token (to re-attach after replacement).
fn trailing_punct(token: &str) -> &str {
    let trimmed = token.trim_end_matches(|c: char| c.is_ascii_punctuation());
    &token[trimmed.len()..]
}

fn ones(word: &str) -> Option<u64> {
    Some(match word {
        "zero" => 0,
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        _ => return None,
    })
}

fn teens(word: &str) -> Option<u64> {
    Some(match word {
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        "thirteen" => 13,
        "fourteen" => 14,
        "fifteen" => 15,
        "sixteen" => 16,
        "seventeen" => 17,
        "eighteen" => 18,
        "nineteen" => 19,
        _ => return None,
    })
}

fn tens(word: &str) -> Option<u64> {
    Some(match word {
        "twenty" => 20,
        "thirty" => 30,
        "forty" => 40,
        "fifty" => 50,
        "sixty" => 60,
        "seventy" => 70,
        "eighty" => 80,
        "ninety" => 90,
        _ => return None,
    })
}

fn month(word: &str) -> Option<&'static str> {
    Some(match word {
        "january" => "January",
        "february" => "February",
        "march" => "March",
        "april" => "April",
        "may" => "May",
        "june" => "June",
        "july" => "July",
        "august" => "August",
        "september" => "September",
        "october" => "October",
        "november" => "November",
        "december" => "December",
        _ => return None,
    })
}

fn ordinal_word(word: &str) -> Option<u64> {
    Some(match word {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "fifth" => 5,
        "sixth" => 6,
        "seventh" => 7,
        "eighth" => 8,
        "ninth" => 9,
        "tenth" => 10,
        "eleventh" => 11,
        "twelfth" => 12,
        "thirteenth" => 13,
        "fourteenth" => 14,
        "fifteenth" => 15,
        "sixteenth" => 16,
        "seventeenth" => 17,
        "eighteenth" => 18,
        "nineteenth" => 19,
        "twentieth" => 20,
        "thirtieth" => 30,
        _ => return None,
    })
}

fn ordinal_suffix(n: u64) -> &'static str {
    match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

#[derive(Debug)]
struct Run {
    value: u64,
    len: usize,
    all_words: bool,
}

/// Parse a maximal valid spoken cardinal starting at `pos`. Digit tokens are
/// accepted only as single-token runs.
fn parse_cardinal(tokens: &[String], pos: usize) -> Option<Run> {
    let first = norm(&tokens[pos]);
    if let Ok(v) = first.parse::<u64>() {
        return Some(Run {
            value: v,
            len: 1,
            all_words: false,
        });
    }

    let mut total: u64 = 0;
    let mut current: u64 = 0;
    // What the sub-hundred part already contains, to reject invalid
    // continuations like "three thirty" (tens after ones).
    let mut has_tens = false;
    let mut has_ones = false;
    let mut i = pos;
    let mut consumed = 0;

    while i < tokens.len() {
        let w = norm(&tokens[i]);
        let next_is_number = |j: usize| {
            tokens.get(j).is_some_and(|t| {
                let n = norm(t);
                ones(&n).is_some() || teens(&n).is_some() || tens(&n).is_some()
            })
        };
        if w == "and" && consumed > 0 && current % 100 == 0 && current > 0 && next_is_number(i + 1)
        {
            i += 1;
            consumed += 1;
            continue;
        }
        if let Some(v) = ones(&w) {
            if has_ones {
                break;
            }
            has_ones = true;
            current += v;
        } else if let Some(v) = teens(&w) {
            if has_tens || has_ones {
                break;
            }
            has_tens = true;
            has_ones = true;
            current += v;
        } else if let Some(v) = tens(&w) {
            if has_tens || has_ones {
                break;
            }
            has_tens = true;
            current += v;
        } else if w == "hundred" {
            if current == 0 || current > 9 || has_tens {
                break;
            }
            current *= 100;
            has_tens = false;
            has_ones = false;
        } else if w == "thousand" {
            if current == 0 {
                break;
            }
            total += current * 1000;
            current = 0;
            has_tens = false;
            has_ones = false;
        } else if w == "million" {
            if current == 0 {
                break;
            }
            total += current * 1_000_000;
            current = 0;
            has_tens = false;
            has_ones = false;
        } else {
            break;
        }
        i += 1;
        consumed += 1;
    }

    if consumed == 0 {
        return None;
    }
    Some(Run {
        value: total + current,
        len: consumed,
        all_words: true,
    })
}

fn is_meridiem(word: &str) -> Option<&'static str> {
    match word.replace('.', "").as_str() {
        "am" => Some("AM"),
        "pm" => Some("PM"),
        _ => None,
    }
}

/// Months that are also verbs: "you may first check", "we march second".
const MONTHS_THAT_ARE_VERBS: &[&str] = &["may", "march"];

/// The pronouns that put the verb, not the month, after them.
const PRONOUNS: &[&str] = &["i", "you", "we", "they", "he", "she", "it"];

/// Words that put a date or a time after them.
const DATE_OR_TIME_LEADS: &[&str] = &[
    "on", "at", "by", "until", "till", "from", "since", "before", "after", "around", "for",
];

/// Whether the token before position `i` asks for a date or a time.
fn led_in(tokens: &[String], i: usize) -> bool {
    i > 0 && DATE_OR_TIME_LEADS.contains(&norm(&tokens[i - 1]).as_str())
}

/// Whether "may" or "march" at `i`, with one ordinal word after it, is the
/// verb: after a pronoun and before "first" or "second", the only ordinals
/// that also follow the verb ("you may first check"). A word in front that
/// asks for a date, or a capital the recognizer gave it in the middle of a
/// sentence, says it is the month.
fn month_is_the_verb(tokens: &[String], i: usize) -> bool {
    let after_pronoun = i > 0 && PRONOUNS.contains(&norm(&tokens[i - 1]).as_str());
    let before_adverb = tokens
        .get(i + 1)
        .is_some_and(|t| matches!(norm(t).as_str(), "first" | "second"));
    let capital_mid_sentence = i > 0
        && tokens[i].starts_with(char::is_uppercase)
        && !tokens[i - 1].ends_with(['.', '!', '?']);
    MONTHS_THAT_ARE_VERBS.contains(&norm(&tokens[i]).as_str())
        && after_pronoun
        && before_adverb
        && !capital_mid_sentence
        && !led_in(tokens, i)
}

/// Whether a bare "am" at `at` is the verb rather than a time: "which one am
/// I", "number one am I right". Only the bare word is in doubt, only before
/// "I", and only when nothing else says this is a time.
fn am_is_the_verb(tokens: &[String], at: usize, hour_at: usize) -> bool {
    tokens[at].eq_ignore_ascii_case("am")
        && tokens.get(at + 1).is_some_and(|t| norm(t) == "i")
        && !led_in(tokens, hour_at)
}

pub fn apply(text: String) -> String {
    let tokens: Vec<String> = text.split_whitespace().map(String::from).collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;

    while i < tokens.len() {
        let w = norm(&tokens[i]);

        // Year: "twenty twenty six" → 2026, "nineteen ninety nine" → 1999.
        if (w == "nineteen" || w == "twenty") && i + 1 < tokens.len() {
            let century = if w == "nineteen" { 1900 } else { 2000 };
            let next = norm(&tokens[i + 1]);
            if tens(&next).is_some() || teens(&next).is_some() {
                if let Some(run) = parse_cardinal(&tokens, i + 1) {
                    if (10..=99).contains(&run.value) {
                        let last = &tokens[i + run.len];
                        out.push(format!(
                            "{}{}",
                            century + run.value,
                            trailing_punct(last)
                        ));
                        i += 1 + run.len;
                        continue;
                    }
                }
            }
        }

        // Date: month + (optional tens) + ordinal word → "January 5th".
        if let Some(m) = month(&w) {
            let mut day: Option<(u64, usize)> = None;
            if let Some(t1) = tokens.get(i + 1).map(|t| norm(t)) {
                if let Some(v) = ordinal_word(&t1) {
                    day = Some((v, 1));
                } else if let (Some(tv), Some(t2)) =
                    (tens(&t1), tokens.get(i + 2).map(|t| norm(t)))
                {
                    if let Some(ov) = ordinal_word(&t2) {
                        if ov <= 9 {
                            day = Some((tv + ov, 2));
                        }
                    }
                }
            }
            // "you may first", "we march second": a verb and an ordinal, not a
            // date. A two-word day ("march twenty second") is a date either
            // way.
            if day.is_some_and(|(_, words)| words == 1) && month_is_the_verb(&tokens, i) {
                day = None;
            }
            if let Some((d, consumed)) = day {
                let last = &tokens[i + consumed];
                out.push(format!(
                    "{m} {d}{}{}",
                    ordinal_suffix(d),
                    trailing_punct(last)
                ));
                i += 1 + consumed;
                continue;
            }
        }

        // Number run, then time / percent / currency / plain cardinal.
        if let Some(run) = parse_cardinal(&tokens, i) {
            let after = i + run.len;

            // Time: hour [minutes] am/pm
            if (1..=12).contains(&run.value) {
                // minutes as a second run
                if let Some(min_run) = tokens
                    .get(after)
                    .and_then(|_| parse_cardinal(&tokens, after))
                {
                    if (1..=59).contains(&min_run.value) {
                        if let Some(mer) = tokens
                            .get(after + min_run.len)
                            .and_then(|t| is_meridiem(&norm(t)))
                        {
                            let last = &tokens[after + min_run.len];
                            out.push(format!(
                                "{}:{:02} {mer}{}",
                                run.value,
                                min_run.value,
                                trailing_punct(last)
                            ));
                            i = after + min_run.len + 1;
                            continue;
                        }
                    }
                }
                // Without minutes, a bare "am" before "I" is the verb.
                if let Some(mer) = tokens
                    .get(after)
                    .and_then(|t| is_meridiem(&norm(t)))
                    .filter(|_| !am_is_the_verb(&tokens, after, i))
                {
                    let last = &tokens[after];
                    out.push(format!("{} {mer}{}", run.value, trailing_punct(last)));
                    i = after + 1;
                    continue;
                }
            }

            // Units
            if let Some(unit) = tokens.get(after).map(|t| norm(t)) {
                let last = &tokens[after];
                let punct = trailing_punct(last);
                match unit.as_str() {
                    "percent" => {
                        out.push(format!("{}%{punct}", run.value));
                        i = after + 1;
                        continue;
                    }
                    "dollars" | "dollar" | "bucks" => {
                        out.push(format!("${}{punct}", run.value));
                        i = after + 1;
                        continue;
                    }
                    "rupees" | "rupee" => {
                        out.push(format!("₹{}{punct}", run.value));
                        i = after + 1;
                        continue;
                    }
                    _ => {}
                }
            }

            // Plain cardinal: small single numbers keep their word form.
            if run.all_words && run.value >= 10 {
                let last = &tokens[i + run.len - 1];
                out.push(format!("{}{}", run.value, trailing_punct(last)));
                i += run.len;
                continue;
            }
        }

        out.push(tokens[i].clone());
        i += 1;
    }

    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "may" and "march" are verbs after a pronoun, and "first" or "second"
    /// after the verb is just an ordinal.
    #[test]
    fn a_month_that_is_also_a_verb_stays_a_word_after_a_pronoun() {
        assert_eq!(apply("you may first check the logs".into()), "you may first check the logs");
        assert_eq!(apply("we march second in the parade".into()), "we march second in the parade");
        assert_eq!(apply("due on may first".into()), "due on May 1st");
        assert_eq!(apply("born on march twenty second".into()), "born on March 22nd");
        assert_eq!(apply("due on january fifth".into()), "due on January 5th");
    }

    /// Nothing has to lead into a date: no one may "fifth", and a dictation
    /// can start with one.
    #[test]
    fn a_date_without_a_word_in_front_is_still_a_date() {
        assert_eq!(apply("the deadline is may fifth".into()), "the deadline is May 5th");
        assert_eq!(apply("May first works".into()), "May 1st works");
        assert_eq!(apply("march second is a monday".into()), "March 2nd is a monday");
    }

    /// The recognizer wrote "May" with a capital in the middle of a
    /// sentence: it heard the month.
    #[test]
    fn a_capitalised_may_mid_sentence_is_the_month() {
        assert_eq!(apply("Thank you May first is fine".into()), "Thank you May 1st is fine");
    }

    /// "am" is the verb in "which one am I", not a time.
    #[test]
    fn am_before_i_is_the_verb_unless_the_time_has_context() {
        assert_eq!(
            apply("which one am i supposed to use".into()),
            "which one am i supposed to use"
        );
        assert_eq!(apply("wake me at ten am".into()), "wake me at 10 AM");
        assert_eq!(apply("it is ten am".into()), "it is 10 AM");
        assert_eq!(
            apply("call me at one am i will be up".into()),
            "call me at 1 AM i will be up"
        );
        assert_eq!(apply("three thirty am i think".into()), "3:30 AM i think");
        assert_eq!(apply("set it for six am i think".into()), "set it for 6 AM i think");
    }
}
