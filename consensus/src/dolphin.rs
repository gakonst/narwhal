// Copyright(C) Facebook, Inc. and its affiliates.
use crate::committer::Committer;
use crate::state::State;
use crate::virtual_state::VirtualState;
use config::{Committee, Stake};
use crypto::{Digest, PublicKey};
use log::{debug, info, log_enabled, warn};
use primary::{Certificate, Round};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{sleep, Duration, Instant};

pub struct Consensus {
    /// The name of this authority.
    name: PublicKey,
    /// The committee information.
    committee: Committee,
    /// The leader timeout value.
    timeout: u64,
    /// The garbage collection depth.
    gc_depth: Round,

    /// Receives new certificates from the primary. The primary should send us new certificates only
    /// if it already sent us its whole history.
    rx_certificate: Receiver<Certificate>,
    /// Outputs the sequence of ordered certificates to the primary (for cleanup and feedback).
    tx_commit: Sender<Certificate>,
    /// Sends the virtual parents to the primary's proposer.
    tx_parents: Sender<(Vec<Digest>, Round)>,
    /// Outputs the sequence of ordered certificates to the application layer.
    tx_output: Sender<Certificate>,

    /// The genesis certificates.
    genesis: Vec<Certificate>,
    /// The virtual dag round to share with the primary.
    virtual_round: Round,
    /// Implements the commit logic and returns an ordered list of certificates.
    committer: Committer,
}

impl Consensus {
    pub fn spawn(
        name: PublicKey,
        committee: Committee,
        timeout: u64,
        gc_depth: Round,
        rx_certificate: Receiver<Certificate>,
        tx_commit: Sender<Certificate>,
        tx_parents: Sender<(Vec<Digest>, Round)>,
        tx_output: Sender<Certificate>,
    ) {
        tokio::spawn(async move {
            Self {
                name,
                committee: committee.clone(),
                timeout,
                gc_depth,
                rx_certificate,
                tx_commit,
                tx_parents,
                tx_output,
                genesis: Certificate::genesis(&committee),
                virtual_round: 1,
                committer: Committer::new(committee),
            }
            .run()
            .await;
        });
    }

    async fn run(&mut self) {
        // The consensus state (everything else is immutable).
        let mut state = State::new(self.gc_depth, self.genesis.clone());
        let mut virtual_state = VirtualState::new(self.committee.clone(), self.genesis.clone());

        let timer = sleep(Duration::from_millis(self.timeout));
        tokio::pin!(timer);

        let mut virtual_round = self.virtual_round;
        let mut quorum = None;
        let mut advance_early = false;
        loop {
            let timer_expired = timer.is_elapsed();
            if (timer_expired || advance_early) && quorum.is_some() {
                // Advance to the next round.
                self.virtual_round = virtual_round + 1;
                debug!("Virtual dag moved to round {}", self.virtual_round);

                // TODO: Needs to also provide the primary::proposer with the virtual parents.
                self.tx_parents
                    .send(quorum.unwrap())
                    .await
                    .expect("Failed to send virtual parents to primary");

                // Reschedule the timer.
                let deadline = Instant::now() + Duration::from_millis(self.timeout);
                timer.as_mut().reset(deadline);

                quorum = None;
                advance_early = false;
            }

            tokio::select! {
                Some(certificate) = self.rx_certificate.recv() => {
                    debug!("Processing {:?}", certificate);
                    virtual_round = certificate.virtual_round();

                    // Add the new certificate to the local storage.
                    state.add(certificate.clone());

                    // Try adding the certificate to the virtual dag.
                    if !virtual_state.try_add(&certificate) {
                        continue;
                    }

                    // Log the latest committed round of every authority (for debug).
                    if log_enabled!(log::Level::Debug) {
                        for (name, round) in &state.last_committed {
                            debug!("Latest commit of {}: Round {}", name, round);
                        }
                    }

                    // Try to commit.
                    let sequence = self.committer.try_commit(&certificate, &mut state, &mut virtual_state);

                    // Output the sequence in the right order.
                    for certificate in sequence {
                        #[cfg(not(feature = "benchmark"))]
                        info!("Committed {}", certificate.header);

                        #[cfg(feature = "benchmark")]
                        for digest in certificate.header.payload.keys() {
                            // NOTE: This log entry is used to compute performance.
                            info!("Committed {} -> {:?}", certificate.header, digest);
                        }

                        self.tx_commit
                            .send(certificate.clone())
                            .await
                            .expect("Failed to send committed certificate to primary");

                        if let Err(e) = self.tx_output.send(certificate).await {
                            warn!("Failed to output certificate: {}", e);
                        }
                    }

                    // Try to advance to the next round.
                    let (parents, authors): (Vec<_>, Vec<_>) = virtual_state
                        .dag
                        .get(&virtual_round)
                        .expect("We just added a certificate with this round")
                        .values()
                        .map(|(digest, x)| (digest.clone(), x.origin()))
                        .collect::<Vec<_>>()
                        .iter()
                        .cloned()
                        .unzip();

                    if authors.iter().any(|x| x == &self.name) {
                        quorum = (authors
                            .iter()
                            .map(|x| self.committee.stake(x))
                            .sum::<Stake>() >= self.committee.quorum_threshold())
                            .then(|| (parents, virtual_round));

                        advance_early = match virtual_round % 2 {
                            0 => self.qc(virtual_round, &virtual_state) || self.tc(virtual_round, &virtual_state),
                            _ => virtual_state.steady_leader(virtual_round).is_some(),
                        };
                    }
                },
                () = &mut timer => {
                    // Nothing to do.
                }
            }
        }
    }

    /// Check if we gathered a quorum of votes for the leader.
    fn qc(&mut self, round: Round, state: &VirtualState) -> bool {
        state.steady_leader(round - 1).map_or_else(
            || false,
            |(leader_digest, _)| {
                state
                    .dag
                    .get(&round)
                    .expect("We just added a certificate with this round")
                    .values()
                    .filter(|(_, x)| x.virtual_parents().contains(&leader_digest))
                    .map(|(_, x)| self.committee.stake(&x.origin()))
                    .sum::<Stake>()
                    >= self.committee.quorum_threshold()
            },
        )
    }

    /// Check if it is impossible to gather a quorum of votes on the leader.
    fn tc(&mut self, round: Round, state: &VirtualState) -> bool {
        state.steady_leader(round - 1).map_or_else(
            || false,
            |(leader_digest, _)| {
                state
                    .dag
                    .get(&round)
                    .expect("We just added a certificate with this round")
                    .values()
                    .filter(|(_, x)| !x.virtual_parents().contains(&leader_digest))
                    .map(|(_, x)| self.committee.stake(&x.origin()))
                    .sum::<Stake>()
                    >= self.committee.validity_threshold()
            },
        )
    }
}