use super::message::TripleMessage;
use crate::types::TripleProtocol;
use crate::util::AffinePointExt;
use cait_sith::protocol::{Action, InitializationError, Participant, ProtocolError};
use cait_sith::triples::{TriplePub, TripleShare};
use k256::Secp256k1;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};

/// Unique number used to identify a specific ongoing triple generation protocol.
/// Without `TripleId` it would be unclear where to route incoming cait-sith triple generation
/// messages.
pub type TripleId = u64;

/// A completed triple.
pub struct Triple {
    pub id: TripleId,
    pub share: TripleShare<Secp256k1>,
    pub public: TriplePub<Secp256k1>,
}

/// An ongoing triple generator.
pub struct TripleGenerator {
    /// Ongoing cait-sith triple generation protocol.
    pub protocol: TripleProtocol,
    /// Whether this triple generation was initiated by the current node.
    pub mine: bool,
}

/// Abstracts how triples are generated by providing a way to request a new triple that will be
/// complete some time in the future and a way to take an already generated triple.
pub struct TripleManager {
    /// Completed unspent triples
    triples: HashMap<TripleId, Triple>,
    /// Ongoing triple generation protocols
    generators: HashMap<TripleId, TripleGenerator>,
    /// List of triple ids generation of which was initiated by the current node.
    mine: VecDeque<TripleId>,

    participants: Vec<Participant>,
    me: Participant,
    threshold: usize,
    epoch: u64,
}

impl TripleManager {
    pub fn new(
        participants: Vec<Participant>,
        me: Participant,
        threshold: usize,
        epoch: u64,
    ) -> Self {
        Self {
            triples: HashMap::new(),
            generators: HashMap::new(),
            mine: VecDeque::new(),
            participants,
            me,
            threshold,
            epoch,
        }
    }

    /// Returns the number of unspent triples available in the manager.
    pub fn len(&self) -> usize {
        self.triples.len()
    }

    /// Returns the number of unspent triples assigned to this node.
    pub fn my_len(&self) -> usize {
        self.mine.len()
    }

    /// Returns the number of unspent triples we will have in the manager once
    /// all ongoing generation protocols complete.
    pub fn potential_len(&self) -> usize {
        self.triples.len() + self.generators.len()
    }

    /// Starts a new Beaver triple generation protocol.
    pub fn generate(&mut self) -> Result<(), InitializationError> {
        let id = rand::random();
        tracing::info!(id, "starting protocol to generate a new triple");
        let protocol: TripleProtocol = Box::new(cait_sith::triples::generate_triple(
            &self.participants,
            self.me,
            self.threshold,
        )?);
        self.generators.insert(
            id,
            TripleGenerator {
                protocol,
                mine: true,
            },
        );
        Ok(())
    }

    /// Take an unspent triple by its id with no way to return it.
    /// It is very important to NOT reuse the same triple twice for two different
    /// protocols.
    pub fn take(&mut self, id: TripleId) -> Option<Triple> {
        self.triples.remove(&id)
    }

    /// Take two random unspent triple generated by this node. Either takes both or none.
    /// It is very important to NOT reuse the same triple twice for two different
    /// protocols.
    pub fn take_mine_twice(&mut self) -> Option<(Triple, Triple)> {
        tracing::info!(mine = ?self.mine, "my triples");
        if self.mine.len() < 2 {
            return None;
        }
        let id0 = self.mine.pop_front()?;
        let id1 = self.mine.pop_front()?;
        tracing::info!(id0, id1, "trying to take two triples");
        if self.triples.contains_key(&id0) && self.triples.contains_key(&id1) {
            Some((
                self.triples.remove(&id0).unwrap(),
                self.triples.remove(&id1).unwrap(),
            ))
        } else {
            tracing::warn!(id0, id1, "my triples are gone");
            None
        }
    }

    /// Ensures that the triple with the given id is either:
    /// 1) Already generated in which case returns `None`, or
    /// 2) Is currently being generated by `protocol` in which case returns `Some(protocol)`, or
    /// 3) Has never been seen by the manager in which case start a new protocol and returns `Some(protocol)`
    // TODO: What if the triple completed generation and is already spent?
    pub fn get_or_generate(
        &mut self,
        id: TripleId,
    ) -> Result<Option<&mut TripleProtocol>, InitializationError> {
        if self.triples.contains_key(&id) {
            Ok(None)
        } else {
            match self.generators.entry(id) {
                Entry::Vacant(e) => {
                    tracing::info!(id, "joining protocol to generate a new triple");
                    let protocol = Box::new(cait_sith::triples::generate_triple(
                        &self.participants,
                        self.me,
                        self.threshold,
                    )?);
                    let generator = e.insert(TripleGenerator {
                        protocol,
                        mine: false,
                    });
                    Ok(Some(&mut generator.protocol))
                }
                Entry::Occupied(e) => Ok(Some(&mut e.into_mut().protocol)),
            }
        }
    }

    /// Pokes all of the ongoing generation protocols and returns a vector of
    /// messages to be sent to the respective participant.
    ///
    /// An empty vector means we cannot progress until we receive a new message.
    pub fn poke(&mut self) -> Result<Vec<(Participant, TripleMessage)>, ProtocolError> {
        let mut messages = Vec::new();
        let mut result = Ok(());
        self.generators.retain(|id, generator| {
            loop {
                let action = match generator.protocol.poke() {
                    Ok(action) => action,
                    Err(e) => {
                        result = Err(e);
                        break false;
                    }
                };
                match action {
                    Action::Wait => {
                        tracing::debug!("waiting");
                        // Retain protocol until we are finished
                        break true;
                    }
                    Action::SendMany(data) => {
                        for p in &self.participants {
                            messages.push((
                                *p,
                                TripleMessage {
                                    id: *id,
                                    epoch: self.epoch,
                                    from: self.me,
                                    data: data.clone(),
                                },
                            ))
                        }
                    }
                    Action::SendPrivate(p, data) => messages.push((
                        p,
                        TripleMessage {
                            id: *id,
                            epoch: self.epoch,
                            from: self.me,
                            data: data.clone(),
                        },
                    )),
                    Action::Return(output) => {
                        tracing::info!(
                            id,
                            big_a = ?output.1.big_a.to_base58(),
                            big_b = ?output.1.big_b.to_base58(),
                            big_c = ?output.1.big_c.to_base58(),
                            "completed triple generation"
                        );
                        self.triples.insert(
                            *id,
                            Triple {
                                id: *id,
                                share: output.0,
                                public: output.1,
                            },
                        );
                        if generator.mine {
                            self.mine.push_back(*id);
                        }
                        // Do not retain the protocol
                        break false;
                    }
                }
            }
        });
        result.map(|_| messages)
    }
}
