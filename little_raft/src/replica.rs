use crate::{
    cluster::Cluster,
    message::{LogEntry, Message},
    state_machine::{Snapshot, StateMachine, StateMachineTransition, TransitionState},
    timer::Timer,
};
use crossbeam_channel::{Receiver, Select};
use rand::Rng;
use std::sync::{Arc, Mutex};
use std::{
    cmp,
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq, Debug)]
enum State {
    Follower,
    Candidate,
    Leader,
}

/// ReplicaID is a type alias used to identify Raft nodes.
pub type ReplicaID = usize;

/// Replica describes the local instance running the Raft algorithm. Its goal is
/// to maintain the consistency of the user-defined StateMachine across the
/// cluster. It uses the user-defined Cluster implementation to talk to other
/// Replicas, be it over the network or pigeon post.
pub struct Replica<T, C, M>
where
    T: StateMachineTransition,
    M: StateMachine<T>,
    C: Cluster<T>,
{
    /// ID of this Replica.
    id: ReplicaID,

    /// IDs of other Replicas in the cluster.
    peer_ids: Vec<ReplicaID>,

    /// User-defined state machine that the cluster Replicates.
    state_machine: Arc<Mutex<M>>,

    /// Interface a Replica uses to communicate with the rest of the cluster.
    cluster: Arc<Mutex<C>>,

    /// Current term.
    current_term: usize,

    /// ID of peers with votes for self.
    current_votes: Option<Box<BTreeSet<usize>>>,

    /// State of this Replica.
    state: State,

    /// Who the last vote was cast for.
    voted_for: Option<usize>,

    /// entries this Replica is aware of.
    log: Vec<LogEntry<T>>,

    /// Index of the highest transition known to be committed.
    commit_index: usize,

    /// Index of the highest transition applied to the local state machine.
    last_applied: usize,

    /// For each server, index of the next log entry to send to that server.
    /// Only present on leaders.
    next_index: BTreeMap<usize, usize>,

    /// For each server, index of highest log entry known to be replicated on
    /// that server. Only present on leaders.
    match_index: BTreeMap<usize, usize>,

    /// No-op transition used to force a faster Replica update when a cluster
    /// Leader changes. Applied this transition multiple times must have no
    /// affect on the state machine.
    noop_transition: T,

    /// Timer used for heartbeat messages.
    heartbeat_timer: Timer,

    /// Timeout range within a randomized timeout is picked for when to start a
    /// new Leader election if the current Leader is not sending heartbeats.
    election_timeout: (Duration, Duration),

    /// If no heartbeat message is received by the deadline, the Replica will
    /// start an election.
    next_election_deadline: Instant,

    snapshot_delta: usize,

    snapshot: Option<Snapshot>,

    index_offset: usize,
}

impl<T, C, M> Replica<T, C, M>
where
    T: StateMachineTransition,
    M: StateMachine<T>,
    C: Cluster<T>,
{
    /// Create a new Replica.
    ///
    /// id is the ID of this Replica within the cluster.
    ///
    /// peer_ids is a vector of IDs of all other Replicas in the cluster.
    ///
    /// cluster represents the abstraction the Replica uses to talk with other
    /// Replicas.
    ///
    /// state_machine is the state machine that Raft maintains.
    ///
    /// noop_transition is a transition that can be applied to the state machine
    /// multiple times with no effect.
    ///
    /// heartbeat_timeout defines how often the Leader Replica sends out
    /// heartbeat messages.
    ///
    /// election_timeout_range defines the election timeout interval. If the
    /// Replica gets no messages from the Leader before the timeout, it
    /// initiates an election.
    ///
    /// In practice, pick election_timeout_range to be 2-3x the value of
    /// heartbeat_timeout, depending on your particular use-case network latency
    /// and responsiveness needs. An election_timeout_range / heartbeat_timeout
    /// ratio that's too low might cause unwarranted re-elections in the
    /// cluster.
    pub fn new(
        id: ReplicaID,
        peer_ids: Vec<ReplicaID>,
        cluster: Arc<Mutex<C>>,
        state_machine: Arc<Mutex<M>>,
        snapshot_delta: usize,
        noop_transition: T,
        heartbeat_timeout: Duration,
        election_timeout_range: (Duration, Duration),
    ) -> Replica<T, C, M> {
        let snapshot = state_machine.lock().unwrap().load_snapshot();
        let index_offset = if let Some(ref snapshot) = snapshot {
            snapshot.last_included_index
        } else {
            0
        };

        Replica {
            state_machine: state_machine,
            cluster: cluster,
            peer_ids: peer_ids,
            id: id,
            current_term: 0,
            current_votes: None,
            state: State::Follower,
            voted_for: None,
            log: vec![LogEntry {
                term: 0,
                index: 0,
                transition: noop_transition.clone(),
            }],
            noop_transition: noop_transition.clone(),
            commit_index: 0,
            last_applied: 0,
            next_index: BTreeMap::new(),
            match_index: BTreeMap::new(),
            election_timeout: election_timeout_range,
            heartbeat_timer: Timer::new(heartbeat_timeout),
            next_election_deadline: Instant::now(),
            snapshot: snapshot,
            snapshot_delta: snapshot_delta,
            index_offset: index_offset,
        }
    }

    /// This function starts the Replica and blocks forever.
    ///
    /// recv_msg is a channel on which the user must notify the Replica whenever
    /// new messages from the Cluster are available. The Replica will not poll
    /// for messages from the Cluster unless notified through recv_msg.
    ///
    /// recv_transition is a channel on which the user must notify the Replica
    /// whenever new transitions to be processed for the StateMachine are
    /// available. The Replica will not poll for pending transitions for the
    /// StateMachine unless notified through recv_transition.
    pub fn start(&mut self, recv_msg: Receiver<()>, recv_transition: Receiver<()>) {
        loop {
            if self.cluster.lock().unwrap().halt() {
                return;
            }

            match self.state {
                State::Leader => self.poll_as_leader(&recv_msg, &recv_transition),
                State::Follower => self.poll_as_follower(&recv_msg),
                State::Candidate => self.poll_as_candidate(&recv_msg),
            }

            self.apply_ready_entries();
        }
    }

    fn poll_as_leader(&mut self, recv_msg: &Receiver<()>, recv_transition: &Receiver<()>) {
        let mut select = Select::new();
        let recv_heartbeat = self.heartbeat_timer.get_rx();
        let (msg, transition, heartbeat) = (
            select.recv(recv_msg),
            select.recv(recv_transition),
            select.recv(recv_heartbeat),
        );

        let oper = select.select();
        match oper.index() {
            // Process pending messages.
            i if i == msg => {
                oper.recv(recv_msg)
                    .expect("could not react to a new message");
                let messages = self.cluster.lock().unwrap().receive_messages();
                for message in messages {
                    self.process_message(message);
                }
            }
            // Process pending transitions.
            i if i == transition => {
                oper.recv(recv_transition)
                    .expect("could not react to a new transition");
                self.load_new_transitions();
                self.broadcast_append_entry_request();
            }
            // Broadcast heartbeat messages.
            i if i == heartbeat => {
                oper.recv(recv_heartbeat)
                    .expect("could not react to the heartbeat");
                self.broadcast_append_entry_request();
                self.heartbeat_timer.renew();
            }
            _ => unreachable!(),
        }
    }

    fn broadcast_append_entry_request(&mut self) {
        self.broadcast_message(|peer_id: ReplicaID| Message::AppendEntryRequest {
            term: self.current_term,
            from_id: self.id,
            prev_log_index: self.next_index[&peer_id] - 1 + self.index_offset,
            prev_log_term: self.log[self.next_index[&peer_id] - 1 - self.index_offset].term,
            entries: self.get_entries_for_peer(peer_id),
            commit_index: self.commit_index,
        });
    }

    fn poll_as_follower(&mut self, recv_msg: &Receiver<()>) {
        match recv_msg.recv_deadline(self.next_election_deadline) {
            // Process pending messages.
            Ok(_) => {
                let messages = self.cluster.lock().unwrap().receive_messages();
                // Update the election deadline if more than zero messages were
                // actually received.
                if messages.len() != 0 {
                    self.update_election_deadline();
                }

                for message in messages {
                    self.process_message(message);
                }
            }
            // Become candidate and update elction deadline.
            _ => {
                self.become_candidate();
                self.update_election_deadline();
            }
        }

        // Load new transitions. The follower will ignore these transitions, but
        // they are still polled for periodically to ensure there are no stale
        // transitions in case the Replica's state changes.
        self.load_new_transitions();
    }

    fn process_message(&mut self, message: Message<T>) {
        match self.state {
            State::Leader => self.process_message_as_leader(message),
            State::Candidate => self.process_message_as_candidate(message),
            State::Follower => self.process_message_as_follower(message),
        }
    }

    fn update_election_deadline(&mut self) {
        // Randomize each election deadline within the allowed range.
        self.next_election_deadline = Instant::now()
            + rand::thread_rng().gen_range(self.election_timeout.0..=self.election_timeout.1);
    }

    fn poll_as_candidate(&mut self, recv_msg: &Receiver<()>) {
        match recv_msg.recv_deadline(self.next_election_deadline) {
            Ok(_) => {
                // Process pending messages.
                let messages = self.cluster.lock().unwrap().receive_messages();
                // Update the election deadline if more than zero messages were
                // actually received.
                if messages.len() != 0 {
                    self.update_election_deadline();
                }
                for message in messages {
                    self.process_message(message);
                }
            }
            // Become candidate and update elction deadline.
            _ => {
                self.become_candidate();
                self.update_election_deadline();
            }
        }

        // Load new transitions. The candidate will ignore these transitions,
        // but they are still polled for periodically to ensure there are no
        // stale transitions in case the Replica's state changes.
        self.load_new_transitions();
    }

    fn broadcast_message<F>(&self, message_generator: F)
    where
        F: Fn(usize) -> Message<T>,
    {
        self.peer_ids.iter().for_each(|peer_id| {
            self.cluster
                .lock()
                .unwrap()
                .send_message(peer_id.clone(), message_generator(peer_id.clone()))
        });
    }

    // Get log entries that have not been acknowledged by the peer.
    fn get_entries_for_peer(&self, peer_id: ReplicaID) -> Vec<LogEntry<T>> {
        self.log[self.next_index[&peer_id] - self.index_offset..self.log.len()].to_vec()
    }

    // Apply entries that are ready to be applied.
    fn apply_ready_entries(&mut self) {
        // Move the commit index to the latest log index that has been
        // replicated on the majority of the replicas.
        let mut state_machine = self.state_machine.lock().unwrap();
        if self.state == State::Leader && self.commit_index < self.log.len() - 1 {
            let mut n = self.log.len() - 1;
            let old_commit_index = self.commit_index;
            while n > self.commit_index {
                let num_replications =
                    self.match_index.iter().fold(
                        0,
                        |acc, mtch_idx| if mtch_idx.1 >= &n { acc + 1 } else { acc },
                    );

                if num_replications * 2 >= self.peer_ids.len()
                    && self.log[n - self.index_offset].term == self.current_term
                {
                    self.commit_index = n;
                }
                n -= 1;
            }

            for i in old_commit_index + 1..=self.commit_index {
                state_machine.register_transition_state(
                    self.log[i - self.index_offset].transition.get_id(),
                    TransitionState::Committed,
                );
            }
        }

        // Apply entries that are behind the currently committed index.
        while self.commit_index > self.last_applied {
            self.last_applied += 1;
            state_machine.apply_transition(
                self.log[self.last_applied - self.index_offset]
                    .transition
                    .clone(),
            );
            state_machine.register_transition_state(
                self.log[self.last_applied - self.index_offset]
                    .transition
                    .get_id(),
                TransitionState::Applied,
            );

            if self.commit_index - self.last_applied == 1 && self.snapshot_delta > 0 {
                let curr_delta = if let Some(snapshot) = &self.snapshot {
                    self.last_applied - snapshot.last_included_index
                } else {
                    self.last_applied
                };
    
                if curr_delta >= self.snapshot_delta {
                    println!("snapshotting!!!");
                    let last_applied = self.last_applied;
                    self.snapshot = Some(state_machine.create_snapshot(
                        last_applied,
                        self.log[last_applied - self.index_offset].term,
                    ));
                    println!("{:?}", self.log);
                    self.log.retain(|l| l.index > last_applied);
                    self.index_offset = last_applied+1;
                    println!("{}", self.index_offset);
                    println!("{:?}", self.log);
                }
            }
        }        
    }

    fn load_new_transitions(&mut self) {
        // Load new transitions. Ignore the transitions if the replica is not
        // the Leader.
        let mut state_machine = self.state_machine.lock().unwrap();
        let transitions = state_machine.get_pending_transitions();
        for transition in transitions {
            if self.state == State::Leader {
                self.log.push(LogEntry {
                    index: self.log.len(),
                    transition: transition.clone(),
                    term: self.current_term,
                });

                state_machine
                    .register_transition_state(transition.get_id(), TransitionState::Queued);
            }
        }
    }

    fn process_message_as_leader(&mut self, message: Message<T>) {
        match message {
            Message::AppendEntryResponse {
                from_id,
                success,
                term,
                last_index,
                mismatch_index,
            } => {
                if term > self.current_term {
                    // Become follower if another node's term is higher.
                    self.cluster.lock().unwrap().register_leader(None);
                    self.become_follower(term);
                } else if success {
                    // Update information about the peer's logs.
                    self.next_index.insert(from_id, last_index + 1);
                    self.match_index.insert(from_id, last_index);
                } else {
                    // Update information about the peer's logs.
                    //
                    // If the mismatch_index is greater than or equal to the
                    // existing next_index, then we know that this rejection is
                    // a stray out-of-order or duplicate rejection, which we can
                    // ignore. The reason we know that is because mismatch_index
                    // is set by the follower to prev_log_index, which was in
                    // turn set by the leader to next_index-1. Hence
                    // mismatch_index can't be greater than or equal to
                    // next_index.
                    //
                    // If the mismatch_index isn't stray, we set next_index to
                    // the min of next_index and last_index; this is equivalent
                    // to the Raft paper's guidance on decreasing next_index by
                    // one at a time, but is more performant in cases when we
                    // can cut straight to the follower's last_index+1.
                    if let Some(mismatch_index) = mismatch_index {
                        if mismatch_index < self.next_index[&from_id] {
                            let next_index = cmp::min(mismatch_index, last_index + 1);
                            self.next_index.insert(from_id, next_index);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn process_vote_request_as_follower(
        &mut self,
        from_id: ReplicaID,
        term: usize,
        last_log_index: usize,
        last_log_term: usize,
    ) {
        if self.current_term > term {
            // Do not vote for Replicas that are behind.
            self.cluster.lock().unwrap().send_message(
                from_id,
                Message::VoteResponse {
                    from_id: self.id,
                    term: self.current_term,
                    vote_granted: false,
                },
            );
        } else if self.current_term < term {
            // Become follower if the other replica's term is higher.
            self.cluster.lock().unwrap().register_leader(None);
            self.become_follower(term);
        }

        if self.voted_for == None || self.voted_for == Some(from_id) {
            if self.log[self.log.len() - 1 - self.index_offset].index <= last_log_index
                && self.log[self.log.len() - 1 - self.index_offset].term <= last_log_term
            {
                // If the criteria are met, grant the vote.
                let mut cluster = self.cluster.lock().unwrap();
                cluster.register_leader(None);
                cluster.send_message(
                    from_id,
                    Message::VoteResponse {
                        from_id: self.id,
                        term: self.current_term,
                        vote_granted: true,
                    },
                );
                self.voted_for = Some(from_id);
            } else {
                // If the criteria are not met, do not grant the vote.
                self.cluster.lock().unwrap().send_message(
                    from_id,
                    Message::VoteResponse {
                        from_id: self.id,
                        term: self.current_term,
                        vote_granted: false,
                    },
                );
            }
        } else {
            // If voted for someone else, don't grant the vote.
            self.cluster.lock().unwrap().send_message(
                from_id,
                Message::VoteResponse {
                    from_id: self.id,
                    term: self.current_term,
                    vote_granted: false,
                },
            )
        }
    }

    fn process_append_entry_request_as_follower(
        &mut self,
        from_id: ReplicaID,
        term: usize,
        prev_log_index: usize,
        prev_log_term: usize,
        entries: Vec<LogEntry<T>>,
        commit_index: usize,
    ) {
        // Check that the leader's term is at least as large as ours.
        if self.current_term > term {
            self.cluster.lock().unwrap().send_message(
                from_id,
                Message::AppendEntryResponse {
                    from_id: self.id,
                    term: self.current_term,
                    success: false,
                    last_index: self.log.len() - 1,
                    mismatch_index: None,
                },
            );
            return;
        // If our log doesn't contain an entry at prev_log_index with the
        // prev_log_term term, reply false.
        } else if prev_log_index >= self.log.len()
            || self.log[prev_log_index - self.index_offset].term != prev_log_term
        {
            self.cluster.lock().unwrap().send_message(
                from_id,
                Message::AppendEntryResponse {
                    from_id: self.id,
                    term: self.current_term,
                    success: false,
                    last_index: self.log.len() + self.index_offset - 1,
                    mismatch_index: Some(prev_log_index),
                },
            );
            return;
        }

        for entry in entries {
            // Drop local inconsistent logs.
            if entry.index < self.log.len() + self.index_offset
                && entry.term != self.log[entry.index - self.index_offset].term
            {
                self.log.truncate(entry.index - self.index_offset);
            }

            // Push received logs.
            if entry.index == self.log.len() + self.index_offset {
                self.log.push(entry);
            }
        }

        // Update local commit index to either the received commit index or the
        // latest local log position, whichever is smaller.
        if commit_index > self.commit_index && self.log.len() != 0 {
            self.commit_index =
                if commit_index < self.log[self.log.len() - 1 - self.index_offset].index {
                    commit_index
                } else {
                    self.log[self.log.len() - 1 - self.index_offset].index
                }
        }

        let mut cluster = self.cluster.lock().unwrap();
        cluster.register_leader(Some(from_id));
        cluster.send_message(
            from_id,
            Message::AppendEntryResponse {
                from_id: self.id,
                term: self.current_term,
                success: true,
                last_index: self.log.len() - 1,
                mismatch_index: None,
            },
        );
    }

    fn process_message_as_follower(&mut self, message: Message<T>) {
        match message {
            Message::VoteRequest {
                from_id,
                term,
                last_log_index,
                last_log_term,
            } => {
                self.process_vote_request_as_follower(from_id, term, last_log_index, last_log_term)
            }
            Message::AppendEntryRequest {
                term,
                from_id,
                prev_log_index,
                prev_log_term,
                entries,
                commit_index,
            } => self.process_append_entry_request_as_follower(
                from_id,
                term,
                prev_log_index,
                prev_log_term,
                entries,
                commit_index,
            ),
            Message::AppendEntryResponse { .. } => { /* ignore */ }
            Message::VoteResponse { .. } => { /* ignore */ }
            _ => {}
        }
    }

    fn process_message_as_candidate(&mut self, message: Message<T>) {
        match message {
            Message::AppendEntryRequest { term, from_id, .. } => {
                self.process_append_entry_request_as_candidate(term, from_id, message)
            }
            Message::VoteRequest { term, from_id, .. } => {
                self.process_vote_request_as_candidate(term, from_id, message)
            }
            Message::VoteResponse {
                from_id,
                term,
                vote_granted,
            } => self.process_vote_response_as_candidate(from_id, term, vote_granted),
            Message::AppendEntryResponse { .. } => { /* ignore */ }
            _ => {}
        }
    }

    fn process_vote_response_as_candidate(
        &mut self,
        from_id: ReplicaID,
        term: usize,
        vote_granted: bool,
    ) {
        if term > self.current_term {
            self.cluster.lock().unwrap().register_leader(None);
            self.become_follower(term);
        } else if vote_granted {
            // Record that the vote has been granted.
            if let Some(cur_votes) = &mut self.current_votes {
                cur_votes.insert(from_id);
                // If more than half of the cluster has voted for the Replica
                // (the Replica itself included), it's time to become the
                // Leader.
                if cur_votes.len() * 2 > self.peer_ids.len() {
                    self.become_leader();
                }
            }
        }
    }

    fn process_vote_request_as_candidate(
        &mut self,
        term: usize,
        from_id: ReplicaID,
        message: Message<T>,
    ) {
        if term > self.current_term {
            self.cluster.lock().unwrap().register_leader(None);
            self.become_follower(term);
            self.process_message(message);
        } else {
            self.cluster.lock().unwrap().send_message(
                from_id,
                Message::VoteResponse {
                    from_id: self.id,
                    term: self.current_term,
                    vote_granted: false,
                },
            );
        }
    }

    fn process_append_entry_request_as_candidate(
        &mut self,
        term: usize,
        from_id: ReplicaID,
        message: Message<T>,
    ) {
        if term >= self.current_term {
            self.cluster.lock().unwrap().register_leader(None);
            self.become_follower(term);
            self.process_message(message);
        } else {
            self.cluster.lock().unwrap().send_message(
                from_id,
                Message::AppendEntryResponse {
                    from_id: self.id,
                    term: self.current_term,
                    success: false,
                    last_index: self.log.len() - 1,
                    mismatch_index: None,
                },
            );
        }
    }

    fn become_leader(&mut self) {
        self.cluster.lock().unwrap().register_leader(Some(self.id));
        self.state = State::Leader;
        self.current_votes = None;
        self.voted_for = None;
        self.next_index = BTreeMap::new();
        self.match_index = BTreeMap::new();
        for peer_id in &self.peer_ids {
            self.next_index.insert(peer_id.clone(), self.log.len());
            self.match_index.insert(peer_id.clone(), 0);
        }

        // If the previous Leader had some uncommitted entries that were
        // replicated to this now-Leader server, this replica will not commit
        // them until its commit index advances to a log entry appended in this
        // Leader's term. To carry out this operation as soon as the new Leader
        // emerges, append a no-op entry. This is a neat optimization described
        // in the part 8 of the paper.
        self.log.push(LogEntry {
            index: self.log.len(),
            transition: self.noop_transition.clone(),
            term: self.current_term,
        });
    }

    fn become_follower(&mut self, term: usize) {
        self.current_term = term;
        self.state = State::Follower;
        self.current_votes = None;
        self.voted_for = None;
    }

    fn become_candidate(&mut self) {
        // Increase current term.
        self.current_term += 1;
        // Claim yourself a candidate.
        self.state = State::Candidate;
        // Initialize votes. Vote for yourself.
        let mut votes = BTreeSet::new();
        votes.insert(self.id);
        self.current_votes = Some(Box::new(votes));
        self.voted_for = Some(self.id);
        // Fan out vote requests.
        self.broadcast_message(|_: usize| Message::VoteRequest {
            from_id: self.id,
            term: self.current_term,
            last_log_index: self.log.len() + self.index_offset - 1,
            last_log_term: self.log[self.log.len() - 1].term,
        });

        if self.peer_ids.len() == 0 {
            self.become_leader();
        }
    }
}
