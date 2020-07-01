use crate::models::unigram::{lattice::Lattice, model::Unigram};
use crate::tokenizer::{AddedToken, Model, Result, Trainer};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;

type SentencePiece = (String, f64);
const SEED_SIZE: usize = 1_000_000;

fn digamma(x: f64) -> f64 {
    let mut x = x;
    let mut result = 0.0;
    while x < 7.0 {
        result -= 1.0 / x;
        x += 1.0;
    }
    x -= 1.0 / 2.0;
    let xx = 1.0 / x;
    let xx2 = xx * xx;
    let xx4 = xx2 * xx2;
    result += x.ln() + (1.0 / 24.0) * xx2 - 7.0 / 960.0 * xx4 + (31.0 / 8064.0) * xx4 * xx2
        - (127.0 / 30720.0) * xx4 * xx4;
    result
}

fn to_log_prob(pieces: &mut [SentencePiece]) {
    let sum: f64 = pieces.iter().map(|(_, score)| score).sum();
    let logsum = sum.ln();
    for (_, score) in pieces.iter_mut() {
        *score = score.ln() - logsum;
    }
}

pub struct UnigramTrainerBuilder {
    show_progress: bool,
}

impl Default for UnigramTrainerBuilder {
    fn default() -> Self {
        UnigramTrainerBuilder {
            show_progress: true,
        }
    }
}

impl UnigramTrainerBuilder {
    pub fn with_progress(mut self, progress: bool) -> Self {
        self.show_progress = progress;
        self
    }

    pub fn build(&self) -> UnigramTrainer {
        UnigramTrainer::new(self.show_progress)
    }
}

pub struct UnigramTrainer {
    show_progress: bool,
    vocab_size: u32,
    n_sub_iterations: u32,
    special_tokens: Vec<AddedToken>,
}

impl Default for UnigramTrainer {
    fn default() -> Self {
        Self {
            show_progress: true,
            vocab_size: 8_000,
            n_sub_iterations: 2,
            special_tokens: vec![],
        }
    }
}

static MAX_PIECE_LENGTH: usize = 16;

fn is_valid_sentencepiece(char_string: &[char]) -> bool {
    // TODO
    // Checks string length, space not in the substring, numbers, hiragana and more
    // https://github.com/google/sentencepiece/blob/26be9516cd81d5315ee31c48d2438018e0eab879/src/trainer_interface.cc#L203
    let n = char_string.len();
    if char_string.is_empty() || n > MAX_PIECE_LENGTH {
        // println!("Too long");
        return false;
    }
    true
    // for (i, c) in char_string.iter().enumerate() {
    //     if *c == ' ' && i > 0 {
    //         // println!("Invalid prefix");
    //         return false;
    //     }
    // }
    // // This function checks that unicode "scripts" are consistent, so we cannot have romaji and
    // // hiragana for instance. Seems pretty specific. Also Hiragana and katakana are mixed

    // true
}

impl UnigramTrainer {
    pub fn new(show_progress: bool) -> UnigramTrainer {
        UnigramTrainer {
            show_progress,
            vocab_size: 8_000,
            n_sub_iterations: 2,
            special_tokens: vec![],
        }
    }

    /// Setup a progress bar if asked to show progress
    fn setup_progress(&self) -> Option<ProgressBar> {
        if self.show_progress {
            let p = ProgressBar::new(0);
            p.set_style(
                ProgressStyle::default_bar()
                    .template("[{elapsed_precise}] {msg:<40!} {wide_bar} {pos:<9!}/{len:>9!}"),
            );
            Some(p)
        } else {
            None
        }
    }

    fn finalize(&self, model: Unigram, required_chars: HashSet<String>) -> Unigram {
        // let mut pieces: Vec<SentencePiece> =
        //     Vec::with_capacity(self.vocab_size.try_into().unwrap());

        let mut min_score_penalty = 0.0;
        let min_score_penalty_delta = 0.0001;

        let mut pieces: HashMap<String, f64> = HashMap::new();
        let existing_pieces: HashMap<&String, f64> = model.iter().collect();
        // XXX: Make sure bos, eos and unk exists and are ids 0, 1, 2
        pieces.insert("<bos>".to_string(), 0.0);
        pieces.insert("<eos>".to_string(), 0.0);
        pieces.insert("<unk>".to_string(), 0.0);
        for c in required_chars {
            if let Some(t) = existing_pieces.get(&c) {
                pieces.insert(c, *t);
            } else {
                let score = model.min_score + min_score_penalty;

                pieces.insert(c, score);
                min_score_penalty += min_score_penalty_delta;
            }
        }
        for (token, score) in model.iter() {
            match pieces.get(token) {
                Some(_) => continue,
                None => pieces.insert(token.to_string(), score),
            };
            if pieces.len() == self.vocab_size as usize {
                break;
            }
        }
        let mut final_pieces: Vec<SentencePiece> = pieces.into_iter().collect();
        final_pieces.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap());
        Unigram::from(&final_pieces, 0, 1, 2)
    }

    fn required_chars(&self, word_counts: &HashMap<String, u32>) -> HashSet<String> {
        // TODO more logic needed if this required chars > vocab_size
        word_counts
            .iter()
            .map(|(s, _count)| s.chars())
            .flatten()
            .map(|c| c.to_string())
            .collect()
    }
    fn make_seed_sentence_pieces(
        &self,
        word_counts: &HashMap<String, u32>,
    ) -> Result<Vec<SentencePiece>> {
        let vocab_size: usize = self.vocab_size.try_into()?;
        let progress = self.setup_progress();
        // Put all sentences in a string, separated by \0
        let total: usize = word_counts
            .iter()
            .map(|(s, _)| s.chars().count())
            .sum::<usize>()
            + word_counts.len();
        let mut flat_string = String::with_capacity(total);
        let mut all_chars: HashMap<char, u32> = HashMap::new();
        let c_sentence_boundary = '\0';
        let k_sentence_boundary = '\0'.to_string();
        for string in word_counts.keys() {
            flat_string.push_str(&string);
            // XXX
            // Comment suggests we add sentence boupiece, but it seems to be missing from actual
            // code.
            flat_string.push_str(&k_sentence_boundary);
            for c in string.chars() {
                if c != c_sentence_boundary {
                    *all_chars.entry(c).or_insert(0) += 1;
                }
            }
        }
        let suffix = esaxx_rs::suffix(&flat_string).unwrap();

        self.update_progress(&progress, vocab_size, "Updating frequent sub strings...");
        //  Basic chars need to be in sentence pieces.
        let mut seed_sentencepieces: Vec<SentencePiece> = vec![];

        let mut sall_chars: Vec<_> = all_chars.into_iter().map(|(a, b)| (b, a)).collect();
        // Reversed order
        sall_chars.sort_by(|a, b| b.cmp(a));
        let mut substr_index: Vec<_> = suffix
            .iter()
            .filter_map(|(string, freq)| {
                if string.len() <= 1 {
                    return None;
                }
                if string.contains(&c_sentence_boundary) {
                    return None;
                }
                if !is_valid_sentencepiece(string) {
                    return None;
                }
                let score = freq * string.len() as u32;
                // if let Some(p) = &progress {
                //     p.inc(1);
                // }
                return Some((score, string));
            })
            .collect();

        // Fill seed_sentencepieces
        println!("all_chars {}", sall_chars.len());
        for (count, character) in sall_chars {
            seed_sentencepieces.push((character.to_string(), count.into()));
            if let Some(p) = &progress {
                p.inc(1);
            }
        }
        println!("substr_index {}", substr_index.len());
        // sort by decreasing score
        substr_index.sort_by(|a, b| b.cmp(a));
        for (score, char_string) in substr_index {
            // Just in case
            assert!(is_valid_sentencepiece(char_string));
            let string: String = char_string.iter().collect();
            seed_sentencepieces.push((string, score.into()));
            if seed_sentencepieces.len() >= SEED_SIZE {
                break;
            }

            // TODO
            // C++ code uses strings, we kept chars
            //assert_eq!(all_chars.get(string), None);
        }
        to_log_prob(&mut seed_sentencepieces);
        self.finalize_progress(&progress, vocab_size);
        Ok(seed_sentencepieces)
    }
    fn prune_sentence_pieces(&self) {
        // TODO
    }

    /// Update the progress bar with the new provided length and message
    fn update_progress(&self, p: &Option<ProgressBar>, len: usize, message: &str) {
        if let Some(p) = p {
            p.set_message(message);
            p.set_length(len as u64);
            p.set_draw_delta(len as u64 / 100);
            p.reset();
        }
    }
    /// Set the progress bar in the finish state
    fn finalize_progress(&self, p: &Option<ProgressBar>, final_len: usize) {
        if let Some(p) = p {
            p.set_length(final_len as u64);
            p.finish();
            println!();
        }
    }

    fn run_e_step(&self, model: &mut Unigram, sentences: &[(String, u32)]) -> (f64, u32, Vec<f64>) {
        let mut expected: Vec<f64> = vec![0.0; model.len()];
        let mut objs: f64 = 0.0;
        let mut ntokens: u32 = 0;

        let all_sentence_freq: u32 = sentences.iter().map(|(_a, b)| *b).sum();

        println!("{} sentences", sentences.len());
        // TODO reparallelize this.
        for (string, freq) in sentences {
            // println!("String {:?} f={}", string, freq);
            // println!("Sentence {}", i);
            // let now = Instant::now();
            let mut lattice = Lattice::from(string, 0, 1, 2);
            // println!("Lattice {:?}", now.elapsed());
            model.populate_nodes(&mut lattice);
            // println!("Populate nodes {:?}", now.elapsed());
            let z: f64 = lattice.populate_marginal(*freq as f64, &mut expected);
            // println!("Populate marginal {:?}", now.elapsed());
            ntokens += lattice.viterbi().len() as u32;
            // println!("Viterbi {:?}", now.elapsed());
            // let mut max = f64::MIN;
            // for score in &expected {
            //     if score > &max {
            //         max = *score;
            //     }
            // }
            // println!("Expected max {:?}", max);
            if z.is_nan() {
                panic!("likelihood is NAN. Input sentence may be too long.");
            }

            objs -= z / (all_sentence_freq as f64);
            // println!("objs {:?}", now.elapsed());
        }

        println!("Obj={} ntokens={}", objs, ntokens);

        (objs, ntokens, expected)
    }
    fn run_m_step(&self, pieces: &[SentencePiece], expected: &[f64]) -> Vec<SentencePiece> {
        if pieces.len() != expected.len() {
            println!("pieces={} expected={}", pieces.len(), expected.len());
            panic!("Those two iterators are supposed to be the same length");
        }
        let mut new_pieces: Vec<SentencePiece> =
            Vec::with_capacity(self.vocab_size.try_into().unwrap());

        let mut sum = 0.0;
        let expected_frequency_threshold = 0.5;
        for (freq, (piece, _)) in expected.iter().zip(pieces) {
            // println!("Freq {}", freq);
            if *freq < expected_frequency_threshold {
                continue;
            }
            new_pieces.push((piece.clone(), *freq));
            sum += freq;
        }
        // // Here we do not use the original EM, but use the
        // // Bayesianified/DPified EM algorithm.
        // // https://cs.stanford.edu/~pliang/papers/tutorial-acl2007-talk.pdf
        // // This modification will act as a sparse prior.
        let logsum = digamma(sum);
        let new_pieces: Vec<_> = new_pieces
            .into_iter()
            .map(|(s, c)| (s, digamma(c) - logsum))
            .collect();
        new_pieces
    }
    pub fn _train(
        &self,
        word_counts: HashMap<String, u32>,
    ) -> Result<(Box<dyn Model>, Vec<AddedToken>)> {
        // TODO handle progress bar.
        let _progress = self.setup_progress();
        //
        // 1. Compute frequent substrings
        // TODO should be either i64 or i32
        let mut pieces: Vec<SentencePiece> =
            Vec::with_capacity(self.vocab_size.try_into().unwrap());
        // XXX: Make sure bos, eos and unk exists and are ids 0, 1, 2
        pieces.push(("<bos>".to_string(), 0.0));
        pieces.push(("<eos>".to_string(), 0.0));
        pieces.push(("<unk>".to_string(), 0.0));
        pieces.extend(self.make_seed_sentence_pieces(&word_counts)?);

        println!("Using {} pieces for EM training", pieces.len());

        let desired_vocab_size: usize = (self.vocab_size as usize * 11) / 10; // * 1.1
        println!(
            "table {} desired vocab {}",
            pieces.len(),
            desired_vocab_size
        );

        let required_chars = self.required_chars(&word_counts);
        // TODO make the model correctly ?
        let mut model = Unigram::from(&pieces, 0, 1, 2);

        let sentences: Vec<_> = word_counts.into_iter().collect();

        loop {
            // Sub-EM iteration.
            for iter in 0..self.n_sub_iterations {
                println!("-------------loop {}", iter);
                // Executes E step
                let (objective, num_tokens, expected) = self.run_e_step(&mut model, &sentences);
                println!("E step expected={}", expected.len());

                // // Executes M step.
                pieces = self.run_m_step(&pieces, &expected);
                // pieces.extend(new_pieces);
                model = Unigram::from(&pieces, 0, 1, 2);
                println!(
                    "Em iter={} size={} obj={} num_tokens={} num_tokens/piece={}",
                    iter,
                    model.len(),
                    objective,
                    num_tokens,
                    num_tokens as f64 / model.len() as f64
                );
            } // end of Sub EM iteration

            // Stops the iteration when the size of sentences reaches to the
            // desired symbol size.
            if pieces.len() <= desired_vocab_size {
                break;
            }

            // Prunes pieces.
            self.prune_sentence_pieces();
        }

        // // Finally, adjusts the size of sentencepices to be |vocab_size|.
        model = self.finalize(model, required_chars);

        Ok((Box::new(model), self.special_tokens.clone()))
    }
}

impl Trainer for UnigramTrainer {
    /// Train a Unigram model
    fn train(
        &self,
        word_counts: HashMap<String, u32>,
    ) -> Result<(Box<dyn Model>, Vec<AddedToken>)> {
        self._train(word_counts)
        // §let (unigram, tokens) = self._train(word_counts)?;
        // §Ok((unigram, tokens))
    }

    /// Process a bunch of tokens, counting them
    fn process_tokens(&self, words: &mut HashMap<String, u32>, tokens: Vec<String>) {
        for token in tokens {
            words
                .entry(token.clone())
                .and_modify(|c| *c += 1)
                .or_insert(1);
        }
    }

    /// Whether we should show progress
    fn should_show_progress(&self) -> bool {
        self.show_progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_approx_eq::assert_approx_eq;

    #[test]
    fn test_unigram_chars() {
        let trainer = UnigramTrainerBuilder::default()
            .with_progress(false)
            .build();
        let mut word_count: HashMap<String, u32> = HashMap::new();
        word_count.insert("This is a".to_string(), 1);
        word_count.insert("こんにちは友達".to_string(), 1);

        let required_chars = trainer.required_chars(&word_count);
        assert_eq!(required_chars.len(), 13);

        let table = trainer.make_seed_sentence_pieces(&word_count).unwrap();

        let target_strings = vec![
            "s", "i", " ", "達", "友", "ん", "は", "に", "ち", "こ", "h", "a", "T", "is ", "s ",
        ];

        let strings: Vec<_> = table.iter().map(|(string, _)| string).collect();
        assert_eq!(strings, target_strings);

        let scores: Vec<_> = table.iter().map(|(_, score)| score).collect();
        let target_scores = vec![
            -2.5649493574615367, // 2.0
            -2.5649493574615367, // 2.0
            -2.5649493574615367, // 2.0
            -3.258096538021482,  // 1.0
            -3.258096538021482,  // 1.0
            -3.258096538021482,  // 1.0
            -3.258096538021482,  // 1.0
            -3.258096538021482,  // 1.0
            -3.258096538021482,  // 1.0
            -3.258096538021482,  // 1.0
            -3.258096538021482,  // 1.0
            -3.258096538021482,  // 1.0
            -3.258096538021482,  // 1.0
            -1.4663370687934272, // 6.0
            -1.8718021769015916, // 4.0
        ];
        println!("Scores {:?}", scores);

        for (score, target_score) in scores.into_iter().zip(target_scores) {
            assert_approx_eq!(*score, target_score, 0.01);
        }
    }

    // #[test]
    // fn test_train_from_file2() {
    //     let trainer = UnigramTrainerBuilder::default()
    //         .with_progress(false)
    //         .build();
    //     let mut word_counts: HashMap<String, u32> = HashMap::new();
    //     let file = read_to_string("data/botchan.txt").unwrap();
    //     for line in file.split('\n') {
    //         word_counts.insert(line.to_string(), 1);
    //     }

    //     // println!("Start train {:?}", word_counts);
    //     let (model, _) = trainer._train(word_counts).unwrap();
    //     println!("Stop train {:?}", model.get_vocab());
    // }

    #[test]
    fn test_to_log_prob() {
        let mut a = vec![("".to_string(), 1.0), ("".to_string(), 2.0)];
        to_log_prob(&mut a);
        let scores = a.iter().map(|(_, score)| *score).collect::<Vec<_>>();
        // ln(1) - ln(3)
        assert_approx_eq!(scores[0], -1.098, 0.01);
        // ln(2) - ln(3)
        assert_approx_eq!(scores[1], -0.405, 0.01);
    }
}