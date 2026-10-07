//! The wake phrase: with the setting on, a plain dictation that speaks to the
//! voice agent by name goes to the agent instead of being typed.
//!
//! A wrong hit costs more than a miss. A miss means saying it again on the
//! agent chord; a wrong hit sends words meant for the document to the agent.
//! So the scan only listens where speech turns to someone, at an *opening*:
//! the first word of the transcript, the first word of a new sentence, or the
//! word after an attention word ("hey", "ok", "अरे", "सुनो"). The name has to
//! be heard there, allowing for the ways speech-to-text spells a name (see
//! [`sound_key`]) and, on a long name only, one slip more. Anywhere else the
//! name is just a word in the dictation.
//!
//! The raw transcript is read once, word by word, and the work at each
//! opening is bounded by the length of the name.
//!
//! PRIVACY: the transcript is the user's words. The one log line, on a hit,
//! carries a character count.

use super::{ChordKind, RouteCtx};
use crate::canonical::{belongs_in_word, fold};

/// English words a speaker puts in front of a name to get an assistant's
/// attention; the word after one is an opening. "hi", "hello" and "dear" are
/// left out: they begin dictated emails and letters ("Hi Priya,"), where the
/// name that follows is the person being written to.
const ATTENTION_WORDS_ENGLISH: &[&str] = &["hey", "ok", "okay"];

/// The Hindi ones. Both lists are always in force, because dictation mixes
/// languages: a Hindi attention word can come before a Latin-script name and
/// an English one before a Devanagari name.
const ATTENTION_WORDS_HINDI: &[&str] = &["अरे", "सुनो", "हे", "ओके"];

/// Marks that end a sentence in this app's languages, all `Sentence_Terminal`
/// in Unicode: the Latin full stop, question and exclamation marks; the danda
/// and double danda, which Hindi, Marathi, Nepali and Sanskrit use and which
/// Bengali, Odia, Assamese, Gujarati and Punjabi text also takes; the
/// Arabic-script full stop and question mark of Urdu, Kashmiri and Sindhi; the
/// Ol Chiki marks of Santali; and the Meetei Mayek mark of Manipuri.
const SENTENCE_ENDS: &[char] = &[
    '.', '?', '!', '\u{0964}', '\u{0965}', '\u{06D4}', '\u{061F}', '\u{1C7E}', '\u{1C7F}',
    '\u{ABEB}',
];

/// What may follow a sentence end before the next sentence starts: closing
/// quotes, straight and curly, and closing brackets.
const CLOSING_MARKS: &[char] = &['"', '\'', '\u{201D}', '\u{2019}', ')', ']'];

/// The shortest name that is listened for, counted in its letters, digits and
/// signs as written. Two characters is the size of the commonest words in
/// this app's languages ("to", "is", "ka", "ko", "jo", "है", "का", "की", "से",
/// "और"), so a two-character name would turn up all through ordinary speech.
/// The count is taken before the sound key evens anything out, so "Bee" and
/// "Zoo" are names even though their keys are two characters long.
const MIN_NAME_LEN: usize = 3;

/// The sound-key length from which a name forgives one slip: a letter or sign
/// added, dropped or changed beyond the spellings [`sound_key`] already evens
/// out.
///
/// Under six, a slip is likelier to be another name or word than a mishearing:
/// "Kiran" and "Karan", "Nova" and "nava", "Arjun" and "Arjan". From six up
/// such neighbours are rare, and a dropped or swapped letter ("butterfy",
/// "sabastian") is most likely the name itself. Never two slips: two would let
/// "fridge" call Friday.
///
/// The sound key and the slip apply only when the name is heard as the same
/// number of words it is written with. A name broken into syllables, or two
/// of its words heard as one, counts only when spelled exactly as written:
/// everyday speech joined up folds into a brief name far too easily ("now a"
/// and Nova, "free day" and Friday).
const SLIP_FORGIVEN_FROM: usize = 6;

// ---------------------------------------------------------------------------
// Hearing the name.
// ---------------------------------------------------------------------------

/// One spelling difference speech-to-text makes in the Indic scripts, evened
/// out on a single character: `None` for a sign that is dropped.
///
/// The nine Brahmic blocks from Devanagari to Malayalam share one layout, so
/// one offset names the same sign in each: the nukta and the virama are
/// dropped (a conjunct and the bare letters then compare equal), the long i and
/// u, as signs and as letters, become the short ones, candrabindu becomes
/// anusvara, and the two other sibilants become sa.
fn even_out_indic(c: char) -> Option<char> {
    let code = u32::from(c);
    if !(0x0900..0x0D80).contains(&code) {
        return Some(c);
    }
    let (block, offset) = (code & !0x7F, code & 0x7F);
    let offset = match offset {
        0x3C | 0x4D => return None,
        0x01 => 0x02,
        0x08 => 0x07,
        0x0A => 0x09,
        0x36 | 0x37 => 0x38,
        0x40 => 0x3F,
        0x42 => 0x41,
        other => other,
    };
    char::from_u32(block | offset)
}

/// Append the sound key of one folded word to `key`.
///
/// A sound key keeps a word's letters, digits and signs, and evens out the
/// spellings speech-to-text gives the same name. In Latin letters: "th" is
/// "t", "w" is "v", "ee" is "i", "oo" is "u", and a doubled letter counts once
/// ("Saraswathi", "Sarasvati" and "Saraswatee" are one key; so are "Pooja"
/// and "Puja"). In the Indic scripts, see [`even_out_indic`].
///
/// Keys are made a word at a time, so a spelling is evened out only inside a
/// word: "saath hi" is not "saathi".
fn sound_key(key: &mut Vec<char>, word: &str) {
    let start = key.len();
    for c in word.chars().filter(|&c| belongs_in_word(c)) {
        let Some(c) = even_out_indic(c) else {
            continue;
        };
        let c = if c == 'w' { 'v' } else { c };
        match (key[start..].last().copied(), c) {
            (Some('t'), 'h') => continue,
            (Some('e'), 'e') | (Some('o'), 'o') => {
                if let Some(vowel) = key.last_mut() {
                    *vowel = if c == 'e' { 'i' } else { 'u' };
                }
                continue;
            }
            _ => {}
        }
        if c.is_ascii_lowercase() && key[start..].last() == Some(&c) {
            continue;
        }
        key.push(c);
    }
}

/// Whether `a` and `b` are the same, or one insertion, deletion or
/// substitution apart.
fn within_one_edit(a: &[char], b: &[char]) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let same = short.iter().zip(long).take_while(|(x, y)| x == y).count();
    if same == short.len() {
        return true;
    }
    let skip = usize::from(short.len() == long.len());
    short[same + skip..] == long[same + 1..]
}

/// One word of the transcript, located by byte offsets into it.
struct Word {
    /// Where the whitespace-delimited token starts, punctuation included.
    start: usize,
    /// Where the compared part starts and ends: the token without the
    /// punctuation stuck to its ends.
    core_start: usize,
    core_end: usize,
    /// The compared part, case-folded and in canonical form.
    key: String,
}

/// The words of `text`, in order. A token that is only punctuation (a dash, a
/// danda set off by spaces) is not a word; its text is part of the gap between
/// the words around it.
fn words(text: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut token_start = None;
    for (at, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (c.is_whitespace(), token_start) {
            (false, None) => token_start = Some(at),
            (true, Some(start)) => {
                token_start = None;
                let token = &text[start..at];
                let Some(lead) = token.find(belongs_in_word) else {
                    continue;
                };
                let tail = token
                    .char_indices()
                    .filter(|&(_, c)| belongs_in_word(c))
                    .map(|(i, c)| i + c.len_utf8())
                    .last()
                    .unwrap_or(token.len());
                out.push(Word {
                    start,
                    core_start: start + lead,
                    core_end: start + tail,
                    key: fold(&token[lead..tail]),
                });
            }
            _ => {}
        }
    }
    out
}

fn is_attention_word(key: &str) -> bool {
    ATTENTION_WORDS_ENGLISH.contains(&key) || ATTENTION_WORDS_HINDI.contains(&key)
}

/// Whether the text between two words ends a sentence: its last mark, after
/// any closing quotes or brackets, is a sentence end.
fn gap_ends_a_sentence(gap: &str) -> bool {
    gap.chars()
        .rev()
        .filter(|c| !c.is_whitespace())
        .find(|c| !CLOSING_MARKS.contains(c))
        .is_some_and(|c| SENTENCE_ENDS.contains(&c))
}

/// Whether word `at` is an opening: the first word, the first word after a
/// sentence end, or the word after an attention word.
fn is_opening(text: &str, words: &[Word], at: usize) -> bool {
    let Some(before) = at.checked_sub(1).map(|i| &words[i]) else {
        return true;
    };
    is_attention_word(&before.key) || gap_ends_a_sentence(&text[before.core_end..words[at].start])
}

/// The configured name, ready to listen for.
struct Name {
    /// Its words' letters, digits and signs, folded and run together.
    spelled: Vec<char>,
    /// Its sound key, made a word at a time.
    key: Vec<char>,
    words: usize,
}

/// Append the letters, digits and signs of one folded word to `spelled`.
fn spell(spelled: &mut Vec<char>, word: &str) {
    spelled.extend(word.chars().filter(|&c| belongs_in_word(c)));
}

impl Name {
    /// `None` for a name that is never listened for: one shorter than
    /// [`MIN_NAME_LEN`], a blank one included.
    fn new(name: &str) -> Option<Name> {
        let words = words(name);
        let (mut spelled, mut key) = (Vec::new(), Vec::new());
        for word in &words {
            spell(&mut spelled, &word.key);
            sound_key(&mut key, &word.key);
        }
        (spelled.len() >= MIN_NAME_LEN).then_some(Name {
            spelled,
            key,
            words: words.len(),
        })
    }

    /// Whether the name is heard starting at word `first`, as the index of
    /// the word it ends on.
    ///
    /// Listens to runs of one word up to one more than the name has, joined
    /// only across spaces, since speech-to-text splits a name with a space
    /// and never with punctuation. Any run spelled as the name is spelled is
    /// the answer. A run of the name's own number of words is also the answer
    /// when its sound key is the name's, or failing that when
    /// [`SLIP_FORGIVEN_FROM`] lets its one slip through and the run is also
    /// one edit from the name as written. Without that second test the folds
    /// and the slip add up, and "sahi ji" would call Saathi Ji.
    fn heard_at(&self, text: &str, words: &[Word], first: usize) -> Option<usize> {
        let forgives = self.key.len() >= SLIP_FORGIVEN_FROM;
        let (mut spelled, mut heard) = (Vec::new(), Vec::new());
        let mut forgiven = None;
        for last in first..words.len().min(first + self.words + 1) {
            if last > first {
                let between = &text[words[last - 1].core_end..words[last].core_start];
                if !between.chars().all(char::is_whitespace) {
                    break;
                }
            }
            spell(&mut spelled, &words[last].key);
            sound_key(&mut heard, &words[last].key);
            if spelled == self.spelled {
                return Some(last);
            }
            if last - first + 1 == self.words {
                if heard == self.key {
                    return Some(last);
                }
                if forgiven.is_none()
                    && forgives
                    && within_one_edit(&heard, &self.key)
                    && within_one_edit(&spelled, &self.spelled)
                {
                    forgiven = Some(last);
                }
            }
            if spelled.len() > self.spelled.len() && heard.len() > self.key.len() + 1 {
                break;
            }
        }
        forgiven
    }
}

/// What the agent should be asked, or `None` when the transcript does not
/// speak to it.
///
/// The first opening where the name is heard decides. When the call opens
/// the transcript, alone or after an attention word, the command is
/// everything after the name, trimmed; punctuation stuck to the name goes
/// with the name. When it comes later, the command is the whole transcript,
/// trimmed, and the agent's brief says how to treat a command spoken inside
/// other text. Nothing but punctuation after the name is no command at all,
/// and the dictation is typed as usual.
pub fn command(transcript: &str, name: &str) -> Option<String> {
    let name = Name::new(name)?;
    let words = words(transcript);
    let (first, last) = (0..words.len())
        .filter(|&at| is_opening(transcript, &words, at))
        .find_map(|at| name.heard_at(transcript, &words, at).map(|last| (at, last)))?;
    let rest = words.get(last + 1)?;
    let call_opens_transcript = first == 0 || (first == 1 && is_attention_word(&words[0].key));
    let command = if call_opens_transcript {
        transcript[rest.start..].trim()
    } else {
        transcript.trim()
    };
    Some(command.to_string())
}

/// The route seam's whole question: does this ordinary dictation carry a
/// command for the agent, and if so what is it?
///
/// Two gates before the matcher runs, and both are structural rather than
/// advisory:
///
/// * **The setting.** Off by default and the reason is in
///   `settings::AgentSettings::wake_word_enabled`: this is the one path where
///   an ordinary dictation can change meaning without the user doing anything
///   differently.
/// * **The chord.** Only the plain push-to-talk chord is scanned. The agent
///   chord is already an agent route and never reaches here. The *translate*
///   chord can: without a key or a chosen output language the translation is
///   skipped, and its words come here as `Route::Cleanup`. They are still
///   not scanned, because words the user asked to have translated are text
///   for the document, whatever they say.
///
/// So the chord is read, not the route: "arrived as `Route::Cleanup`" and
/// "the user asked for a plain dictation" are different claims, and only the
/// second one may be scanned.
pub fn upgrade(raw: &str, ctx: &RouteCtx<'_>) -> Option<String> {
    if !ctx.settings.agent.wake_word_enabled || ctx.chord != ChordKind::Dictation {
        return None;
    }
    let command = command(raw, ctx.agent_name)?;
    // PRIVACY: a length, not the words. Logged only on a hit — a line per
    // dictation would be a log of how often the user says nothing to the
    // agent, which is both noise and, in aggregate, content.
    tracing::debug!(
        command_chars = command.chars().count(),
        "wake word addressed the agent"
    );
    Some(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped agent name.
    const NAME: &str = "Butterfly";

    fn cmd(transcript: &str) -> Option<String> {
        command(transcript, NAME)
    }

    /// Whether `spoken`, said first and followed by a request, calls `name`.
    fn calls(spoken: &str, name: &str) -> bool {
        command(&format!("{spoken} open the report"), name).is_some()
    }

    // -- English dictation ---------------------------------------------------

    mod english {
        use super::*;

        #[test]
        fn a_name_that_opens_the_dictation_hands_over_the_rest() {
            for transcript in [
                "Butterfly, draft a reply to Meena",
                "Butterfly: draft a reply to Meena",
                "Butterfly draft a reply to Meena",
                "Butterfly , draft a reply to Meena",
                "  Butterfly, draft a reply to Meena  ",
            ] {
                assert_eq!(
                    cmd(transcript).as_deref(),
                    Some("draft a reply to Meena"),
                    "{transcript:?}"
                );
            }
        }

        /// Later in the transcript, the whole of it goes to the agent, and the
        /// agent's brief sorts out which part is the request.
        #[test]
        fn a_name_starting_a_new_sentence_hands_over_the_whole_transcript() {
            for end in [".", "?", "!"] {
                for closer in ["", "\"", "'", ")", "]", "\u{201D}", "\u{2019}"] {
                    let transcript =
                        format!("We met the vendor today{end}{closer} Butterfly make a summary");
                    assert_eq!(
                        cmd(&transcript).as_deref(),
                        Some(transcript.as_str()),
                        "{transcript:?}"
                    );
                }
            }
            assert_eq!(
                cmd("  Invoices are due on Friday. Butterfly, make that a reminder  ").as_deref(),
                Some("Invoices are due on Friday. Butterfly, make that a reminder")
            );
        }

        #[test]
        fn hey_ok_and_okay_turn_the_next_word_into_a_call() {
            for cue in ["hey", "Hey,", "OK", "okay"] {
                let transcript = format!("{cue} Butterfly summarise this thread");
                assert_eq!(
                    cmd(&transcript).as_deref(),
                    Some("summarise this thread"),
                    "{transcript:?}"
                );
            }
        }

        /// "Dear", "Hello" and "Hi" open letters and emails, where the name
        /// after them is the person written to.
        #[test]
        fn letter_and_email_openings_are_not_calls() {
            for transcript in [
                "Dear Butterfly team, thanks for the flowers",
                "Hello Butterfly, how are you",
                "Hi Butterfly, the parcel reached Pune",
            ] {
                assert_eq!(cmd(transcript), None, "{transcript:?}");
            }
        }

        #[test]
        fn a_name_inside_a_sentence_is_part_of_the_dictation() {
            for transcript in [
                "I saw a Butterfly near the window today",
                "she said \"Butterfly is late\" again",
                "the Butterfly garden opens on Monday",
                "the butter fly was lovely",
            ] {
                assert_eq!(cmd(transcript), None, "{transcript:?}");
            }
        }

        #[test]
        fn a_call_after_a_mention_still_counts() {
            let transcript = "The Butterfly logo looks good. Butterfly, make that sound warmer";
            assert_eq!(cmd(transcript).as_deref(), Some(transcript));
        }
    }

    // -- Hindi dictation, and Hindi mixed with English ------------------------

    mod hindi {
        use super::*;

        const NAME_HI: &str = "बटरफ्लाई";

        #[test]
        fn every_hindi_attention_word_turns_the_next_word_into_a_call() {
            for cue in ["अरे", "सुनो", "हे", "ओके"] {
                let opening = format!("{cue} बटरफ्लाई इसे छोटा करो");
                assert_eq!(
                    command(&opening, NAME_HI).as_deref(),
                    Some("इसे छोटा करो"),
                    "{cue}"
                );
                let later = format!("तो फिर {cue} बटरफ्लाई इसे छोटा करो");
                assert_eq!(
                    command(&later, NAME_HI).as_deref(),
                    Some(later.as_str()),
                    "{cue} after other words"
                );
            }
        }

        #[test]
        fn a_danda_or_double_danda_ends_the_sentence_before_the_name() {
            for transcript in [
                "काम हो गया। बटरफ्लाई सारांश बनाओ",
                "काम हो गया । बटरफ्लाई सारांश बनाओ",
                "श्लोक यहाँ समाप्त होता है॥ बटरफ्लाई इसका अर्थ लिखो",
            ] {
                assert_eq!(
                    command(transcript, NAME_HI).as_deref(),
                    Some(transcript),
                    "{transcript}"
                );
            }
        }

        #[test]
        fn hindi_and_english_mix_in_either_direction() {
            assert_eq!(
                command("OK बटरफ्लाई इसे छोटा करो", NAME_HI).as_deref(),
                Some("इसे छोटा करो"),
                "an English attention word before a Devanagari name"
            );
            assert_eq!(
                cmd("सुनो Butterfly, list the pending invoices").as_deref(),
                Some("list the pending invoices"),
                "a Hindi attention word before a Latin-script name"
            );
            let glued = "बैठक खत्म हो गई। Butterfly इसका सारांश बनाओ";
            assert_eq!(cmd(glued).as_deref(), Some(glued), "a danda before a Latin-script name");
        }

        #[test]
        fn punctuation_on_the_name_or_the_attention_word_goes_with_it() {
            for transcript in [
                "अरे, बटरफ्लाई, इसे छोटा करो",
                "अरे, बटरफ्लाई इसे छोटा करो",
                "अरे बटरफ्लाई, इसे छोटा करो",
                "बटरफ्लाई। इसे छोटा करो",
            ] {
                assert_eq!(
                    command(transcript, NAME_HI).as_deref(),
                    Some("इसे छोटा करो"),
                    "{transcript}"
                );
            }
        }

        #[test]
        fn a_name_inside_a_hindi_sentence_is_part_of_the_dictation() {
            for transcript in ["मैंने बगीचे में बटरफ्लाई देखी", "मैंने बटरफ्लाई के बारे में सुना"] {
                assert_eq!(command(transcript, NAME_HI), None, "{transcript}");
            }
        }
    }

    // -- Hinglish: Hindi dictated in Latin letters ----------------------------

    mod hinglish {
        use super::*;

        #[test]
        fn a_hinglish_request_after_the_name_is_handed_over() {
            assert_eq!(
                command("Saathi, kal ki meeting ka reminder laga do", "Saathi").as_deref(),
                Some("kal ki meeting ka reminder laga do")
            );
            assert_eq!(
                command("hey Saathi mummy ko message bhejo", "Saathi").as_deref(),
                Some("mummy ko message bhejo")
            );
        }

        #[test]
        fn a_name_inside_a_hinglish_sentence_is_part_of_the_dictation() {
            assert_eq!(cmd("kal garden mein ek Butterfly dekhi thi"), None);
            assert_eq!(command("main apne saathi ke saath gaya tha", "Saathi"), None);
        }

        /// "saath hi" ("along with that") sounds like the name run together,
        /// but keys are made a word at a time; "saathiyon" is a longer word.
        #[test]
        fn hindi_words_that_sound_close_to_the_name_do_not_call() {
            for transcript in ["saath hi ye bhi likh do", "Saathiyon, aaj ki baat suno", "Saath chalo"] {
                assert_eq!(command(transcript, "Saathi"), None, "{transcript}");
            }
        }
    }

    // -- Speech-to-text output without punctuation ----------------------------

    mod unpunctuated {
        use super::*;

        #[test]
        fn a_name_alone_at_the_start_still_calls() {
            assert_eq!(
                cmd("butterfly delete that paragraph").as_deref(),
                Some("delete that paragraph")
            );
        }

        /// Speech-to-text often leaves sentence ends out; an attention word
        /// still marks a call wherever it stands.
        #[test]
        fn an_attention_word_opens_a_call_anywhere() {
            let transcript = "The notes are done hey Butterfly tidy them up";
            assert_eq!(cmd(transcript).as_deref(), Some(transcript));
        }

        /// With neither a sentence end nor an attention word before it, a
        /// name later in the transcript is not at an opening.
        #[test]
        fn without_a_sentence_end_a_later_name_is_not_a_call() {
            assert_eq!(cmd("We met the vendor today Butterfly make a summary"), None);
        }
    }

    // -- How speech-to-text spells a name ------------------------------------

    mod spellings {
        use super::*;

        fn assert_all_call(cases: &[(&str, &[&str])]) {
            for (name, heard) in cases {
                for spoken in *heard {
                    assert!(calls(spoken, name), "{spoken:?} should call {name:?}");
                }
            }
        }

        fn assert_none_call(cases: &[(&str, &[&str])]) {
            for (name, heard) in cases {
                for spoken in *heard {
                    assert!(!calls(spoken, name), "{spoken:?} should not call {name:?}");
                }
            }
        }

        #[test]
        fn latin_spellings_of_one_name_are_heard_as_it() {
            assert_all_call(&[
                ("Saathi", &["saathi", "Sathi", "saathee", "Saati", "Saathi!"]),
                ("Saraswati", &["saraswathi", "sarasvati", "saraswatee", "Saraswati,"]),
                ("Preeti", &["priti", "preethi", "Preetti"]),
                ("Ananya", &["anannya", "ananyaa"]),
                ("Pooja", &["puja", "poojaa"]),
                ("Vishwa", &["vishva", "Vishwaa"]),
            ]);
        }

        /// The nine Brahmic scripts share one layout, so the same rule evens
        /// out vowel-sign length, virama, nukta and sibilants in each.
        #[test]
        fn indic_spellings_of_one_name_are_heard_as_it() {
            assert_all_call(&[
                ("बटरफ्लाई", &["बटरफ्लाइ", "बटरफलाई", "बटरफ़्लाई", "बटरफ्लाई।"]),
                ("सरस्वती", &["शरस्वती", "सरस्वति", "सरसवती"]),
                ("साथी", &["साथि", "साथी,"]),
                ("কাবেরী", &["কাবেরি"]),
                ("கீதா", &["கிதா"]),
                ("ശ്രീലക്ഷ്മി", &["ശ്രിലക്ഷ്മി", "ശ്രീലക്ഷമി"]),
            ]);
        }

        #[test]
        fn a_name_split_or_run_together_is_heard_whole() {
            assert_eq!(
                cmd("butter fly, draft the minutes").as_deref(),
                Some("draft the minutes")
            );
            assert_eq!(
                cmd("Butter-fly, draft the minutes").as_deref(),
                Some("draft the minutes")
            );
            assert_eq!(
                command("बटर फ्लाई इसे छोटा करो", "बटरफ्लाई").as_deref(),
                Some("इसे छोटा करो")
            );
            assert_all_call(&[
                ("Sebastian", &["seb astian"]),
                ("Captain Nova", &["captain nova", "captainnova", "Captain Nova"]),
                ("Saathi", &["saa thi"]),
            ]);
        }

        #[test]
        fn a_long_name_forgives_one_slip() {
            assert_all_call(&[
                ("Butterfly", &["butterfy", "butterfli", "buterfly"]),
                ("Sebastian", &["sabastian", "sebastien"]),
                ("Aurora", &["arora", "aurorah"]),
                ("Captain Nova", &["captain nava"]),
                ("सरस्वती", &["सरस्वाती"]),
            ]);
        }

        /// On a short name a slip is likelier to be another name or word.
        #[test]
        fn a_short_name_forgives_no_slip() {
            assert_none_call(&[
                ("Kiran", &["karan", "kiren"]),
                ("Nova", &["nava"]),
                ("Arjun", &["arjan"]),
                ("Max", &["mix"]),
                ("Saathi", &["sathe", "sachi"]),
                ("सखी", &["सखा"]),
            ]);
        }

        /// Everyday words run together can fold into a short name, so a run
        /// of more words than the name has must spell it exactly.
        #[test]
        fn everyday_words_run_together_do_not_call() {
            for (transcript, name) in [
                ("Now a few points on the budget", "Nova"),
                ("Sat I was there", "Saathi"),
                ("Free day tomorrow, let's go out", "Friday"),
            ] {
                assert_eq!(command(transcript, name), None, "{transcript:?} called {name:?}");
            }
        }

        /// A two-word name forgives a slip only when the phrase is one edit
        /// from the name as written, not just once both are folded.
        #[test]
        fn a_folded_near_miss_on_a_two_word_name_does_not_call() {
            for (transcript, name) in [
                ("sahi ji, ye kaam ho gaya", "Saathi Ji"),
                ("chhota bhi chalega", "Chhota Bheem"),
            ] {
                assert_eq!(command(transcript, name), None, "{transcript:?} called {name:?}");
            }
            assert!(calls("captain nava", "Captain Nova"), "one slip as written still calls");
        }

        #[test]
        fn a_split_name_gets_no_slip_and_no_name_gets_two() {
            assert_none_call(&[
                ("Butterfly", &["butter fy"]),
                ("Sebastian", &["seb astien", "sabastien"]),
                ("Captain Nova", &["caption nova"]),
                ("Friday", &["fridge"]),
            ]);
        }

        #[test]
        fn the_name_is_never_heard_inside_a_longer_word() {
            assert_eq!(command("Maxwell sent the file", "Max"), None);
            assert_eq!(command("Maximum effort today", "Max"), None);
            assert_eq!(
                command("Max, open the file", "Max").as_deref(),
                Some("open the file")
            );
        }

        /// Names and transcripts are compared in canonical form, so a name
        /// typed with a precomposed letter matches speech-to-text output that
        /// spells it with a combining mark.
        #[test]
        fn a_precomposed_name_matches_a_decomposed_transcript() {
            assert_eq!(
                command("Zoe\u{0308}, play some music", "Zo\u{00EB}").as_deref(),
                Some("play some music")
            );
        }
    }

    // -- Names that are never listened for -----------------------------------

    mod unusable_names {
        use super::*;

        #[test]
        fn a_name_needs_three_letters_as_written() {
            for name in ["", "   ", "\t", "Al", "जी"] {
                assert_eq!(
                    command(&format!("{name}, open the file"), name),
                    None,
                    "{name:?}"
                );
            }
            assert_eq!(
                command("Ava, open the file", "Ava").as_deref(),
                Some("open the file"),
                "three is enough"
            );
        }

        /// The length is counted before the sound key evens the name out, so
        /// a short name whose key is shorter still is listened for.
        #[test]
        fn a_short_name_with_a_shorter_sound_key_still_calls() {
            for name in ["Bee", "Lee", "Zoo"] {
                assert_eq!(
                    command(&format!("{name}, open the file"), name).as_deref(),
                    Some("open the file"),
                    "{name}"
                );
            }
        }
    }

    // -- What the agent is asked ---------------------------------------------

    mod the_request {
        use super::*;

        #[test]
        fn the_request_keeps_its_own_spacing_and_punctuation() {
            assert_eq!(
                cmd("Butterfly,   write  it   up, then \"send\" it.  ").as_deref(),
                Some("write  it   up, then \"send\" it.")
            );
        }

        #[test]
        fn a_split_name_comes_off_whole() {
            assert_eq!(cmd("butter fly write it up").as_deref(), Some("write it up"));
        }

        #[test]
        fn the_name_with_nothing_after_it_is_no_request() {
            for transcript in [
                "Butterfly",
                "Butterfly.",
                "hey Butterfly",
                "Thanks for waiting. Butterfly!",
            ] {
                assert_eq!(cmd(transcript), None, "{transcript:?}");
            }
            assert_eq!(command("अरे बटरफ्लाई।", "बटरफ्लाई"), None);
        }
    }

    // -- Everyday words ------------------------------------------------------

    mod everyday_words {
        use super::*;

        /// Names people give an assistant, against everyday words in this
        /// app's languages: no word on the list may call any of them.
        #[test]
        fn no_everyday_word_calls_a_listed_name() {
            let names = [
                "Butterfly", "Saathi", "Friday", "Kiran", "Nova", "Max", "Aurora", "Sebastian",
                "Mitra", "Ananya", "Saraswati", "Vidya", "Kavya", "Sakhi", "Arjun", "Captain Nova",
                "बटरफ्लाई", "साथी", "सरस्वती", "मित्रा", "सखी",
            ];
            let everyday = "a i an am as at be by do go he hi if in is it me my no of oh ok on or \
                so to up us we the and but you yes hey for not all now then that this with what \
                when there here they from your good fine well please thanks okay hello butter fly \
                flies better bitter jar service nervous fridge never maximum arrow friend sat \
                saath sath sathe ab ek ha ho ja ji jo ka ke ki ko na ne pe se tu vo ye \
                hai aur yeh woh haan nahi toh bhi tha kar karo kya abhi bas theek accha main hum \
                tum aap tera mera है का की के को से और यह वह तो भी था थी एक कर जी ही पर ने हो जो \
                में हाँ नहीं तुम हम आप क्या अभी बस ठीक अच्छा मैं तेरा मेरा मित्र सखा साथ बटर जार सर सरस";
            for name in names {
                for word in everyday.split_whitespace() {
                    assert!(!calls(word, name), "the everyday word {word:?} called {name:?}");
                }
            }
        }
    }

    // -- The pieces ----------------------------------------------------------

    mod pieces {
        use super::*;

        fn key_of(word: &str) -> String {
            let mut key = Vec::new();
            sound_key(&mut key, &fold(word));
            key.into_iter().collect()
        }

        fn chars(s: &str) -> Vec<char> {
            s.chars().collect()
        }

        #[test]
        fn spellings_of_one_name_share_a_sound_key() {
            let groups: [&[&str]; 5] = [
                &["Saathi", "Sathi", "saathee"],
                &["Saraswati", "Saraswathi", "Sarasvati", "Saraswatee"],
                &["Preeti", "Priti", "Preethi"],
                &["Pooja", "Puja"],
                &["Butter-fly", "Butterfly", "buterfly"],
            ];
            for group in groups {
                for spelling in group {
                    assert_eq!(key_of(spelling), key_of(group[0]), "{spelling} against {}", group[0]);
                }
            }
            assert_eq!(key_of("Saathi"), "sati");
        }

        #[test]
        fn one_offset_names_the_same_sign_in_every_brahmic_script() {
            // Virama in Devanagari, Bengali, Tamil, Telugu and Malayalam.
            for virama in ['\u{094D}', '\u{09CD}', '\u{0BCD}', '\u{0C4D}', '\u{0D4D}'] {
                assert_eq!(even_out_indic(virama), None, "U+{:04X}", u32::from(virama));
            }
            // The long i sign becomes the short one.
            for (long, short) in [
                ('\u{0940}', '\u{093F}'),
                ('\u{09C0}', '\u{09BF}'),
                ('\u{0BC0}', '\u{0BBF}'),
                ('\u{0C40}', '\u{0C3F}'),
                ('\u{0D40}', '\u{0D3F}'),
            ] {
                assert_eq!(even_out_indic(long), Some(short), "U+{:04X}", u32::from(long));
            }
            // Letters outside those blocks, and the ones not evened out, stay.
            for c in ['a', 'क', 'ক', 'ஸ', '\u{0DA7}'] {
                assert_eq!(even_out_indic(c), Some(c));
            }
        }

        #[test]
        fn within_one_edit_allows_one_change_and_no_more() {
            assert!(within_one_edit(&chars("monsoon"), &chars("monsoon")));
            assert!(within_one_edit(&chars("monsoon"), &chars("monson")));
            assert!(within_one_edit(&chars("ghat"), &chars("ghats")));
            assert!(within_one_edit(&chars("kettle"), &chars("settle")));
            assert!(within_one_edit(&chars(""), &chars("a")));
            assert!(!within_one_edit(&chars("parcel"), &chars("pencil")));
            assert!(!within_one_edit(&chars("ghat"), &chars("ghatsx")));
            assert!(!within_one_edit(&chars("chai"), &chars("chia")));
        }
    }
}
