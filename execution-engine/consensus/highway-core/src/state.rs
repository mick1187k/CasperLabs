use std::{collections::HashMap, ops::Mul};

use derive_more::{Add, AddAssign, Sub, SubAssign, Sum};
use displaydoc::Display;
use thiserror::Error;

use crate::{
    block::Block,
    evidence::Evidence,
    tallies::Tallies,
    traits::Context,
    validators::ValidatorIndex,
    vertex::{Dependency, WireVote},
    vote::{Observation, Panorama, Vote},
};

/// A vote weight.
#[derive(
    Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Add, Sub, AddAssign, SubAssign, Sum,
)]
pub struct Weight(pub u64);

impl Mul<u64> for Weight {
    type Output = Self;

    fn mul(self, rhs: u64) -> Self {
        Weight(self.0 * rhs)
    }
}

/// An error that occurred when trying to add a vote.
#[derive(Debug, Error)]
#[error("{:?}", .cause)]
pub struct AddVoteError<C: Context> {
    /// The invalid vote that was not added to the protocol state.
    pub wvote: WireVote<C>,
    /// The reason the vote is invalid.
    #[source]
    pub cause: VoteError,
}

#[derive(Debug, Display, Error, PartialEq)]
pub enum VoteError {
    /// The vote's panorama is inconsistent.
    Panorama,
    /// The vote contains the wrong sequence number.
    SequenceNumber,
}

impl<C: Context> WireVote<C> {
    fn with_error(self, cause: VoteError) -> AddVoteError<C> {
        AddVoteError { wvote: self, cause }
    }
}

/// A passive instance of the Highway protocol, containing its local state.
///
/// Both observers and active validators must instantiate this, pass in all incoming vertices from
/// peers, and use a [FinalityDetector](../finality_detector/struct.FinalityDetector.html) to
/// determine the outcome of the consensus process.
#[derive(Debug)]
pub struct State<C: Context> {
    /// The validator's voting weights.
    weights: Vec<Weight>,
    /// All votes imported so far, by hash.
    // TODO: HashMaps prevent deterministic tests.
    votes: HashMap<C::VoteHash, Vote<C>>,
    /// All blocks, by hash.
    blocks: HashMap<C::VoteHash, Block<C>>,
    /// Evidence to prove a validator malicious, by index.
    evidence: HashMap<ValidatorIndex, Evidence<C>>,
    /// The full panorama, corresponding to the complete protocol state.
    panorama: Panorama<C>,
}

impl<C: Context> State<C> {
    pub fn new(weights: &[Weight]) -> State<C> {
        State {
            weights: weights.to_vec(),
            votes: HashMap::new(),
            blocks: HashMap::new(),
            evidence: HashMap::new(),
            panorama: Panorama::new(weights.len()),
        }
    }

    /// Returns evidence against validator nr. `idx`, if present.
    pub fn opt_evidence(&self, idx: ValidatorIndex) -> Option<&Evidence<C>> {
        self.evidence.get(&idx)
    }

    /// Returns whether evidence against validator nr. `idx` is known.
    pub fn has_evidence(&self, idx: ValidatorIndex) -> bool {
        self.evidence.contains_key(&idx)
    }

    /// Returns the vote with the given hash, if present.
    pub fn opt_vote(&self, hash: &C::VoteHash) -> Option<&Vote<C>> {
        self.votes.get(hash)
    }

    /// Returns whether the vote with the given hash is known.
    pub fn has_vote(&self, hash: &C::VoteHash) -> bool {
        self.votes.contains_key(hash)
    }

    /// Returns the vote with the given hash. Panics if not found.
    pub fn vote(&self, hash: &C::VoteHash) -> &Vote<C> {
        self.opt_vote(hash).unwrap()
    }

    /// Returns the block contained in the vote with the given hash, if present.
    pub fn opt_block(&self, hash: &C::VoteHash) -> Option<&Block<C>> {
        self.blocks.get(hash)
    }

    /// Returns the block contained in the vote with the given hash. Panics if not found.
    pub fn block(&self, hash: &C::VoteHash) -> &Block<C> {
        self.opt_block(hash).unwrap()
    }

    /// Adds the vote to the protocol state, or returns an error if it is invalid.
    /// Panics if dependencies are not satisfied.
    pub fn add_vote(&mut self, wvote: WireVote<C>) -> Result<(), AddVoteError<C>> {
        if let Err(err) = self.validate_vote(&wvote) {
            return Err(wvote.with_error(err));
        }
        self.update_panorama(&wvote);
        let hash = wvote.hash.clone();
        let fork_choice = self.fork_choice(&wvote.panorama).cloned();
        let (vote, opt_values) = Vote::new(wvote, fork_choice.as_ref(), self);
        if let Some(values) = opt_values {
            let block = Block::new(fork_choice, values, self);
            self.blocks.insert(hash.clone(), block);
        }
        self.votes.insert(hash, vote);
        Ok(())
    }

    pub fn add_evidence(&mut self, evidence: Evidence<C>) {
        let idx = evidence.perpetrator();
        self.evidence.insert(idx, evidence);
    }

    pub fn wire_vote(&self, hash: C::VoteHash) -> Option<WireVote<C>> {
        let vote = self.opt_vote(&hash)?.clone();
        let opt_block = self.opt_block(&hash);
        let values = opt_block.map(|block| block.values.clone());
        Some(WireVote {
            hash,
            panorama: vote.panorama.clone(),
            sender: vote.sender,
            values,
            seq_number: vote.seq_number,
        })
    }

    /// Returns the first missing dependency of the panorama, or `None` if all are satisfied.
    pub fn missing_dependency(&self, panorama: &Panorama<C>) -> Option<Dependency<C>> {
        let missing_dep = |(idx, obs)| self.missing_obs_dep(idx, obs);
        panorama.enumerate().filter_map(missing_dep).next()
    }

    /// Returns an error if `wvote` is invalid.
    fn validate_vote(&self, wvote: &WireVote<C>) -> Result<(), VoteError> {
        let sender = wvote.sender;
        // Check that the panorama is consistent.
        if (wvote.values.is_none() && wvote.panorama.is_empty())
            || !self.is_panorama_valid(&wvote.panorama)
        {
            return Err(VoteError::Panorama);
        }
        // Check that the vote's sequence number is one more than the sender's previous one.
        let expected_seq_number = match wvote.panorama.get(sender) {
            Observation::Faulty => return Err(VoteError::Panorama),
            Observation::None => 0,
            Observation::Correct(hash) => 1 + self.vote(hash).seq_number,
        };
        if wvote.seq_number != expected_seq_number {
            return Err(VoteError::SequenceNumber);
        }
        Ok(())
    }

    /// Update `self.panorama` with an incoming vote. Panics if dependencies are missing.
    ///
    /// If the new vote is valid, it will just add `Observation::Correct(wvote.hash)` to the
    /// panorama. If it represents an equivocation, it adds `Observation::Faulty` and updates
    /// `self.evidence`.
    fn update_panorama(&mut self, wvote: &WireVote<C>) {
        let sender = wvote.sender;
        let new_obs = match (self.panorama.get(sender), wvote.panorama.get(sender)) {
            (Observation::Faulty, _) => Observation::Faulty,
            (obs0, obs1) if obs0 == obs1 => Observation::Correct(wvote.hash.clone()),
            (Observation::None, _) => panic!("missing own previous vote"),
            (Observation::Correct(hash0), _) => {
                if !self.has_evidence(sender) {
                    let prev0 = self.find_in_swimlane(hash0, wvote.seq_number);
                    let wvote0 = self.wire_vote(prev0.clone()).unwrap();
                    self.add_evidence(Evidence::Equivocation(wvote0, wvote.clone()));
                }
                Observation::Faulty
            }
        };
        self.panorama.update(wvote.sender, new_obs);
    }

    /// Returns the fork choice from `pan`'s view, or `None` if there are no blocks yet.
    ///
    /// The correct validators' latest votes count as votes for the block they point to, as well as
    /// all of its ancestors. At each level the block with the highest score is selected from the
    /// children of the previously selected block (or from all blocks at height 0), until a block
    /// is reached that has no children with any votes.
    fn fork_choice<'a>(&'a self, pan: &Panorama<C>) -> Option<&'a C::VoteHash> {
        // Collect all correct votes in a `Tallies` map, sorted by height.
        let to_entry = |(obs, w): (&Observation<C>, &Weight)| {
            let bhash = &self.vote(obs.correct()?).block;
            Some((self.block(bhash).height, bhash, *w))
        };
        let mut tallies: Tallies<C> = pan.iter().zip(&self.weights).filter_map(to_entry).collect();
        loop {
            // Find the highest block that we know is an ancestor of the fork choice.
            let (height, bhash) = tallies.find_decided(self)?;
            // Drop all votes that are not descendants of `bhash`.
            tallies = tallies.filter_descendants(height, bhash, self);
            // If there are no blocks left, `bhash` itself is the fork choice. Otherwise repeat.
            if tallies.is_empty() {
                return Some(bhash);
            }
        }
    }

    /// Returns the hash of the message with the given sequence number from the sender of `hash`.
    /// Panics if the sequence number is higher than that of the vote with `hash`.
    fn find_in_swimlane<'a>(&'a self, hash: &'a C::VoteHash, seq_number: u64) -> &'a C::VoteHash {
        let vote = self.vote(hash);
        if vote.seq_number == seq_number {
            return hash;
        }
        assert!(vote.seq_number > seq_number);
        let diff = vote.seq_number - seq_number;
        // We want to make the greatest step 2^i such that 2^i <= diff.
        let max_i = log2(diff) as usize;
        let i = max_i.min(vote.skip_idx.len() - 1);
        self.find_in_swimlane(&vote.skip_idx[i], seq_number)
    }

    /// Returns the ancestor of the block with the given `hash`, on the specified `height`, or
    /// `None` if the block's height is lower than that.
    pub fn find_ancestor<'a>(
        &'a self,
        hash: &'a C::VoteHash,
        height: u64,
    ) -> Option<&'a C::VoteHash> {
        let block = self.block(hash);
        if block.height < height {
            return None;
        }
        if block.height == height {
            return Some(hash);
        }
        let diff = block.height - height;
        // We want to make the greatest step 2^i such that 2^i <= diff.
        let max_i = log2(diff) as usize;
        let i = max_i.min(block.skip_idx.len() - 1);
        self.find_ancestor(&block.skip_idx[i], height)
    }

    /// Returns `pan` is valid, i.e. it contains the latest votes of some substate of `self`.
    fn is_panorama_valid(&self, pan: &Panorama<C>) -> bool {
        pan.enumerate().all(|(idx, observation)| {
            match observation {
                Observation::None => true,
                Observation::Faulty => self.has_evidence(idx),
                Observation::Correct(hash) => match self.opt_vote(hash) {
                    Some(vote) => vote.sender == idx && self.panorama_geq(pan, &vote.panorama),
                    None => false, // Unknown vote. Not a substate of `state`.
                },
            }
        })
    }

    /// Returns whether `pan_l` can possibly come later in time than `pan_r`, i.e. it can see
    /// every honest message and every fault seen by `other`.
    fn panorama_geq(&self, pan_l: &Panorama<C>, pan_r: &Panorama<C>) -> bool {
        let mut pairs_iter = pan_l.0.iter().zip(&pan_r.0);
        pairs_iter.all(|(obs_l, obs_r)| self.obs_geq(obs_l, obs_r))
    }

    /// Returns `true` if `pan` sees the sender of `hash` as correct, and sees that vote.
    fn sees_correct(&self, pan: &Panorama<C>, hash: &C::VoteHash) -> bool {
        let vote = self.vote(hash);
        pan.get(vote.sender).correct().map_or(false, |latest_hash| {
            hash == self.find_in_swimlane(latest_hash, vote.seq_number)
        })
    }

    /// Returns whether `obs_l` can come later in time than `obs_r`.
    fn obs_geq(&self, obs_l: &Observation<C>, obs_r: &Observation<C>) -> bool {
        match (obs_l, obs_r) {
            (Observation::Faulty, _) | (_, Observation::None) => true,
            (Observation::Correct(hash0), Observation::Correct(hash1)) => {
                hash0 == hash1 || self.sees_correct(&self.vote(hash0).panorama, hash1)
            }
            (_, _) => false,
        }
    }

    /// Returns the missing dependency if `obs` is referring to a vertex we don't know yet.
    fn missing_obs_dep(&self, idx: ValidatorIndex, obs: &Observation<C>) -> Option<Dependency<C>> {
        match obs {
            Observation::Faulty if !self.has_evidence(idx) => Some(Dependency::Evidence(idx)),
            Observation::Correct(hash) if !self.has_vote(hash) => {
                Some(Dependency::Vote(hash.clone()))
            }
            _ => None,
        }
    }
}

/// Returns the base-2 logarithm of `x`, rounded down,
/// i.e. the greatest `i` such that `2.pow(i) <= x`.
fn log2(x: u64) -> u32 {
    // The least power of two that is strictly greater than x.
    let next_pow2 = (x + 1).next_power_of_two();
    // It's twice as big as the greatest power of two that is less or equal than x.
    let prev_pow2 = next_pow2 >> 1;
    // The number of trailing zeros is its base-2 logarithm.
    prev_pow2.trailing_zeros()
}

#[cfg(test)]
pub mod tests {
    use crate::traits::ValidatorSecret;

    use super::*;

    pub const WEIGHTS: &[Weight] = &[Weight(3), Weight(4), Weight(5)];

    pub const ALICE: ValidatorIndex = ValidatorIndex(0);
    pub const BOB: ValidatorIndex = ValidatorIndex(1);
    pub const CAROL: ValidatorIndex = ValidatorIndex(2);

    #[derive(Clone, Debug, PartialEq)]
    pub struct TestContext;

    #[derive(Debug)]
    pub struct TestSecret(u64);

    impl ValidatorSecret for TestSecret {
        type Signature = u64;

        fn sign(&self, _data: &[u8]) -> Vec<u8> {
            unimplemented!()
        }
    }

    impl Context for TestContext {
        type ConsensusValue = &'static str;
        type ValidatorId = &'static str;
        type ValidatorSecret = TestSecret;
        type VoteHash = &'static str;
        type InstanceId = &'static str;
    }

    /// Converts a string to an observation: "F" means faulty, "_" means none, and other strings
    /// are used as the identifier ("hash") of a correct vote.
    pub fn to_obs(s: &&'static str) -> Observation<TestContext> {
        match *s {
            "_" => Observation::None,
            "F" => Observation::Faulty,
            s => Observation::Correct(s),
        }
    }

    /// Creates a panorama based on observation descriptions as in `to_obs`.
    pub fn panorama(observations: [&'static str; 3]) -> Panorama<TestContext> {
        Panorama(observations.iter().map(to_obs).collect())
    }

    /// Creates a new ballot vote. The hash must be a letter, followed by the sequence number.
    pub fn vote(
        hash: &'static str,
        sender: ValidatorIndex,
        observations: [&'static str; 3],
    ) -> WireVote<TestContext> {
        WireVote {
            hash,
            panorama: panorama(observations),
            sender,
            values: None,
            seq_number: hash[1..].parse().unwrap(),
        }
    }

    impl WireVote<TestContext> {
        /// Adds a value to the vote, turning it into a new block.
        pub fn val(mut self, value: &'static str) -> Self {
            self.values = match self.values {
                None => Some(vec![value]),
                Some(mut values) => {
                    values.push(value);
                    Some(values)
                }
            };
            self
        }
    }

    /// Returns the cause of the error, dropping the `WireVote`.
    fn vote_err(err: AddVoteError<TestContext>) -> VoteError {
        err.cause
    }

    #[test]
    fn add_vote() -> Result<(), AddVoteError<TestContext>> {
        let mut state = State::new(WEIGHTS);

        // Create votes as follows; a0, b0 are blocks:
        //
        // Alice: a0 ————— a1
        //                /
        // Bob:   b0 —— b1
        //          \  /
        // Carol:    c0
        state.add_vote(vote("a0", ALICE, ["_", "_", "_"]).val("a"))?;
        state.add_vote(vote("b0", BOB, ["_", "_", "_"]).val("b"))?;
        state.add_vote(vote("c0", CAROL, ["_", "b0", "_"]))?;
        state.add_vote(vote("b1", BOB, ["_", "b0", "c0"]))?;
        state.add_vote(vote("a1", ALICE, ["a0", "b1", "c0"]))?;

        // Wrong sequence number: Carol hasn't produced c1 yet.
        let opt_err = state.add_vote(vote("c2", CAROL, ["_", "b1", "c0"])).err();
        assert_eq!(Some(VoteError::SequenceNumber), opt_err.map(vote_err));
        // Inconsistent panorama: If you see b1, you have to see c0, too.
        let opt_err = state.add_vote(vote("c1", CAROL, ["_", "b1", "_"])).err();
        assert_eq!(Some(VoteError::Panorama), opt_err.map(vote_err));

        // Alice has not equivocated yet, and not produced message A1.
        let missing = state.missing_dependency(&panorama(["F", "b1", "c0"]));
        assert_eq!(Some(Dependency::Evidence(ALICE)), missing);
        let missing = state.missing_dependency(&panorama(["A1", "b1", "c0"]));
        assert_eq!(Some(Dependency::Vote("A1")), missing);

        // Alice equivocates: A1 doesn't see a1.
        state.add_vote(vote("A1", ALICE, ["a0", "b1", "c0"]))?;
        assert!(state.has_evidence(ALICE));

        let missing = state.missing_dependency(&panorama(["F", "b1", "c0"]));
        assert_eq!(None, missing);
        let missing = state.missing_dependency(&panorama(["A1", "b1", "c0"]));
        assert_eq!(None, missing);

        // Bob can see the equivocation.
        state.add_vote(vote("b2", BOB, ["F", "b1", "c0"]))?;

        // The state's own panorama has been updated correctly.
        assert_eq!(state.panorama, panorama(["F", "b2", "c0"]));
        Ok(())
    }

    #[test]
    fn find_in_swimlane() -> Result<(), AddVoteError<TestContext>> {
        let mut state = State::new(WEIGHTS);
        let a = ["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9"];
        state.add_vote(vote(a[0], ALICE, ["_", "_", "_"]).val("a"))?;
        for i in 1..a.len() {
            state.add_vote(vote(a[i], ALICE, [a[i - 1], "_", "_"]))?;
        }

        // The predecessor with sequence number i should always equal a[i].
        for j in (a.len() - 2)..a.len() {
            for i in 0..j {
                assert_eq!(&a[i], state.find_in_swimlane(&a[j], i as u64));
            }
        }

        // The skip list index of a[k] includes a[k - 2^i] for each i such that 2^i divides k.
        assert_eq!(&["a8"], &state.vote(&"a9").skip_idx.as_ref());
        assert_eq!(
            &["a7", "a6", "a4", "a0"],
            &state.vote(&"a8").skip_idx.as_ref()
        );
        Ok(())
    }

    #[test]
    fn fork_choice() -> Result<(), AddVoteError<TestContext>> {
        let mut state = State::new(WEIGHTS);

        // Create blocks with scores as follows:
        //
        //          a0: 7 — a1: 3
        //        /       \
        // b0: 12           b2: 4
        //        \
        //          c0: 5 — c1: 5
        state.add_vote(vote("b0", BOB, ["_", "_", "_"]).val("B0"))?;
        state.add_vote(vote("c0", CAROL, ["_", "b0", "_"]).val("C0"))?;
        state.add_vote(vote("c1", CAROL, ["_", "b0", "c0"]).val("C1"))?;
        state.add_vote(vote("a0", ALICE, ["_", "b0", "_"]).val("A0"))?;
        state.add_vote(vote("b1", BOB, ["a0", "b0", "_"]))?; // Just a ballot; not shown above.
        state.add_vote(vote("a1", ALICE, ["a0", "b1", "c1"]).val("A1"))?;
        state.add_vote(vote("b2", BOB, ["a0", "b1", "_"]).val("B2"))?;

        // Alice built `a1` on top of `a0`, which had already 7 points.
        assert_eq!(Some(&"a0"), state.block(&state.vote(&"a1").block).parent());
        // The fork choice is now `b2`: At height 1, `a0` wins against `c0`.
        // At height 2, `b2` wins against `a1`. `c1` has most points but is not a child of `a0`.
        assert_eq!(Some(&"b2"), state.fork_choice(&state.panorama));
        Ok(())
    }

    #[test]
    fn test_log2() {
        assert_eq!(2, log2(0b100));
        assert_eq!(2, log2(0b101));
        assert_eq!(2, log2(0b111));
        assert_eq!(3, log2(0b1000));
    }
}
